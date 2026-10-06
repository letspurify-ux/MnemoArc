//! Manually enabled ad hoc SQL and PL/SQL execution.
use super::{DatabaseConfig, collect_rows, connect, identifier, remaining};
use anyhow::{Result, bail};
use oracle::{
    Connection, SqlValue, Statement,
    sql_type::{OracleType, RefCursor, ToSql, ToSqlNull},
};
use serde_json::{Map, Value, json};
use std::{collections::BTreeSet, time::Instant};
use tokio_util::sync::CancellationToken;

fn bad(message: &str) -> anyhow::Error {
    anyhow::anyhow!("invalid_database_execution_arguments: {message}")
}

fn bad_at(field: &str, error: anyhow::Error) -> anyhow::Error {
    let message = error.to_string();
    let detail = message
        .strip_prefix("invalid_database_execution_arguments: ")
        .unwrap_or(&message);
    bad(&format!("{field}: {detail}"))
}

fn safe_name(name: &str) -> bool {
    let pieces: Vec<_> = name.split('.').collect();
    (1..=3).contains(&pieces.len())
        && pieces.iter().all(|piece| {
            let mut chars = piece.chars();
            chars.next().is_some_and(|c| c.is_ascii_alphabetic())
                && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'))
        })
}

fn sql_text(args: &Value) -> Result<&str> {
    let sql = args["sql"]
        .as_str()
        .ok_or_else(|| bad("sql is required"))?
        .trim();
    if sql.is_empty() || sql.len() > 32768 {
        bail!("invalid_database_execution_arguments: sql must contain 1 to 32768 bytes");
    }
    Ok(sql)
}

fn validate_shape(args: &Value, required: &[&str], allowed: &[&str]) -> Result<()> {
    let fields = args
        .as_object()
        .ok_or_else(|| bad("arguments must be an object"))?;
    for field in required {
        if !fields.contains_key(*field) {
            bail!("invalid_database_execution_arguments: {field} is required");
        }
    }
    for field in fields.keys() {
        if !allowed.contains(&field.as_str()) {
            bail!("invalid_database_execution_arguments: {field} is not allowed for this mode");
        }
    }
    Ok(())
}

enum Input {
    Text(Option<String>),
    Number(Option<String>),
    Bool(Option<bool>),
}

impl ToSql for Input {
    fn oratype(&self, conn: &Connection) -> std::result::Result<OracleType, oracle::Error> {
        match self {
            Self::Text(Some(value)) => value.oratype(conn),
            Self::Text(None) => String::oratype_for_null(conn),
            Self::Number(_) => Ok(OracleType::Number(0, -127)),
            Self::Bool(_) => Ok(OracleType::Boolean),
        }
    }

    fn to_sql(&self, sql_value: &mut SqlValue) -> std::result::Result<(), oracle::Error> {
        match self {
            Self::Text(Some(text)) | Self::Number(Some(text)) => Ok(text.to_sql(sql_value)?),
            Self::Text(None) | Self::Number(None) => sql_value.set_null(),
            Self::Bool(input) => Ok(input.to_sql(sql_value)?),
        }
    }
}

impl Input {
    fn bind(&self, stmt: &mut Statement, name: &str, direction: &str, kind: &str) -> Result<()> {
        if direction == "in" {
            match self {
                Self::Number(value) => {
                    let oracle_type = OracleType::Number(0, -127);
                    stmt.bind(name, &(value, &oracle_type))?;
                }
                _ => stmt.bind(name, self)?,
            }
            return Ok(());
        }
        if direction == "out" {
            match kind {
                "cursor" => stmt.bind(name, &None::<RefCursor>)?,
                _ => stmt.bind(name, &output_type(kind)?)?,
            }
            return Ok(());
        }
        let oracle_type = output_type(kind)?;
        match self {
            Self::Text(value) => stmt.bind(name, &(value, &oracle_type))?,
            Self::Number(value) => stmt.bind(name, &(value, &oracle_type))?,
            Self::Bool(value) => stmt.bind(name, &(value, &oracle_type))?,
        }
        Ok(())
    }
}

fn input(value: &Value, kind: &str) -> Result<Input> {
    match kind {
        "boolean" => match value {
            Value::Null => Ok(Input::Bool(None)),
            Value::Bool(value) => Ok(Input::Bool(Some(*value))),
            _ => Err(bad("boolean bind value must be true, false or null")),
        },
        "string" => {
            let value = match value {
                Value::Null => None,
                Value::String(value) => Some(value.clone()),
                Value::Number(value) => Some(value.to_string()),
                _ => return Err(bad("bind value must be string, number or null")),
            };
            if value.as_ref().is_some_and(|v| v.len() > 8192) {
                return Err(bad("bind value exceeds 8192 bytes"));
            }
            Ok(Input::Text(value))
        }
        "number" => {
            let value = match value {
                Value::Null => None,
                Value::Number(value) => Some(value.to_string()),
                Value::String(value) => {
                    if value.len() > 8192 {
                        return Err(bad("bind value exceeds 8192 bytes"));
                    }
                    // Validate decimal syntax without passing through f64.
                    // Oracle NUMBER accepts more significant digits than a
                    // JSON floating-point number can retain.
                    static DECIMAL: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
                    let decimal = DECIMAL.get_or_init(|| {
                        regex::Regex::new(
                            r"\A-?(?:0|[1-9][0-9]*)(?:\.[0-9]+)?(?:[eE][+-]?[0-9]+)?\z",
                        )
                        .expect("decimal bind syntax")
                    });
                    if !decimal.is_match(value) {
                        return Err(bad("number bind value must be a valid decimal or null"));
                    }
                    Some(value.clone())
                }
                _ => return Err(bad("number bind value must be a number or null")),
            };
            if value.as_ref().is_some_and(|v| v.len() > 8192) {
                return Err(bad("bind value exceeds 8192 bytes"));
            }
            Ok(Input::Number(value))
        }
        _ => Err(bad("type must be string, number, boolean or cursor")),
    }
}

fn sql_binds(args: &Value) -> Result<Vec<(String, Input)>> {
    let Some(params) = args.get("params") else {
        return Ok(vec![]);
    };
    let params = params
        .as_object()
        .ok_or_else(|| bad("params must be an object"))?;
    if params.len() > 32 {
        return Err(bad("at most 32 bind parameters are supported"));
    }
    let mut names = BTreeSet::new();
    params.iter().map(|(name, value)| {
        if !identifier(name) { return Err(bad(&format!("params.{name}: bind names must begin with a letter and contain only letters, numbers or underscores"))); }
        if !names.insert(name.to_ascii_lowercase()) {
            return Err(bad(&format!("params.{name}: bind names must be unique ignoring case")));
        }
        let kind = match value {
            Value::Bool(_) => "boolean",
            Value::Number(_) => "number",
            _ => "string",
        };
        let value = input(value, kind).map_err(|error| bad_at(&format!("params.{name}"), error))?;
        Ok((name.clone(), value))
    }).collect()
}

fn output_type(kind: &str) -> Result<OracleType> {
    match kind {
        "string" => Ok(OracleType::Varchar2(4000)),
        "number" => Ok(OracleType::Number(0, -127)),
        "boolean" => Ok(OracleType::Boolean),
        "cursor" => Ok(OracleType::RefCursor),
        _ => Err(bad("type must be string, number, boolean or cursor")),
    }
}

struct CallArg {
    name: String,
    direction: String,
    kind: String,
    input: Input,
}

fn call_args(args: &Value) -> Result<Vec<CallArg>> {
    let Some(raw) = args.get("args") else {
        return Ok(vec![]);
    };
    let raw = raw.as_array().ok_or_else(|| bad("args must be an array"))?;
    if raw.len() > 32 {
        return Err(bad("at most 32 procedure/function arguments are supported"));
    }
    let mut names = BTreeSet::new();
    raw.iter()
        .enumerate()
        .map(|(index, entry)| {
            let parsed = (|| -> Result<CallArg> {
                let entry = entry
                    .as_object()
                    .ok_or_else(|| bad("each args item must be an object"))?;
                for key in entry.keys() {
                    if !["name", "direction", "type", "value"].contains(&key.as_str()) {
                        return Err(bad(
                            "args items accept only name, direction, type and value",
                        ));
                    }
                }
                let name = entry
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| bad("each argument needs name"))?;
                if !identifier(name)
                    || name.eq_ignore_ascii_case("mnemoarc_result")
                    || !names.insert(name.to_ascii_lowercase())
                {
                    return Err(bad(
                        "argument names must be unique valid bind names and cannot be mnemoarc_result",
                    ));
                }
                let direction = match entry.get("direction") {
                    None => "in",
                    Some(value) => value
                        .as_str()
                        .ok_or_else(|| bad("direction must be a string: in, out or inout"))?,
                };
                if !["in", "out", "inout"].contains(&direction) {
                    return Err(bad("direction must be in, out or inout"));
                }
                let kind = match entry.get("type") {
                    None => "string",
                    Some(value) => value.as_str().ok_or_else(|| {
                        bad("type must be a string: string, number, boolean or cursor")
                    })?,
                };
                output_type(kind)?;
                if kind == "cursor" && direction != "out" {
                    return Err(bad("cursor is supported only as an OUT argument"));
                }
                if direction == "out" && entry.contains_key("value") {
                    return Err(bad("OUT arguments must omit value"));
                }
                if direction != "out" && !entry.contains_key("value") {
                    return Err(bad("IN and INOUT arguments need value (null is allowed)"));
                }
                let input = if direction == "out" {
                    Input::Text(None)
                } else {
                    input(&entry["value"], kind)?
                };
                Ok(CallArg {
                    name: name.into(),
                    direction: direction.into(),
                    kind: kind.into(),
                    input,
                })
            })();
            parsed.map_err(|error| bad_at(&format!("args[{index}]"), error))
        })
        .collect()
}

fn read_output(
    stmt: &Statement,
    name: &str,
    kind: &str,
    conn: &Connection,
    config: &DatabaseConfig,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value> {
    match kind {
        "boolean" => Ok(json!(stmt.bind_value::<_, Option<bool>>(name)?)),
        "cursor" => {
            let cursor: Option<RefCursor> = stmt.bind_value(name)?;
            let Some(mut cursor) = cursor else {
                return Ok(Value::Null);
            };
            let mut rows = cursor.query()?;
            collect_rows(conn, &mut rows, config, cancel, deadline)
        }
        _ => Ok(json!(stmt.bind_value::<_, Option<String>>(name)?)),
    }
}

fn rollback_failure(cause: anyhow::Error, rollback: Result<()>) -> anyhow::Error {
    match rollback {
        Ok(()) => cause,
        Err(error) => anyhow::anyhow!(
            "database_rollback_uncertain: operation failed: {cause}; rollback failed: {error}"
        ),
    }
}

fn finish_mutation(
    conn: &Connection,
    operation: Result<Value>,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value> {
    let mut result = match operation {
        Ok(result) => result,
        Err(error) => {
            return Err(rollback_failure(error, conn.rollback().map_err(Into::into)));
        }
    };
    if cancel.is_cancelled() {
        return Err(rollback_failure(
            anyhow::anyhow!("cancelled"),
            conn.rollback().map_err(Into::into),
        ));
    }
    if let Err(error) =
        remaining(deadline).and_then(|left| conn.set_call_timeout(Some(left)).map_err(Into::into))
    {
        return Err(rollback_failure(error, conn.rollback().map_err(Into::into)));
    }
    conn.commit()
        .map_err(|error| anyhow::anyhow!("database_commit_uncertain: {error}"))?;
    result["committed"] = json!(true);
    Ok(result)
}

pub fn execute_free(
    config: &DatabaseConfig,
    args: &Value,
    cancel: &CancellationToken,
    timeout_secs: u64,
) -> Result<Value> {
    if !config.enabled {
        bail!("database_disabled: enable database access manually in Settings");
    }
    let mode = args["mode"]
        .as_str()
        .ok_or_else(|| bad("mode is required"))?;
    let allowed = match mode {
        "query" => config.raw_query_enabled,
        "statement" => config.raw_statement_enabled,
        "procedure" => config.procedure_enabled,
        "function" => config.function_enabled,
        _ => return Err(bad("mode must be query, statement, procedure or function")),
    };
    if !allowed {
        bail!("database_execution_disabled: enable this execution mode manually in Settings");
    }
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let deadline = crate::config::deadline_after(timeout_secs)?;
    match mode {
        "query" | "statement" => {
            validate_shape(args, &["mode", "sql"], &["mode", "sql", "params"])?;
            let sql = sql_text(args)?;
            if mode == "query"
                && !matches!(
                    sql.split_whitespace()
                        .next()
                        .map(str::to_ascii_uppercase)
                        .as_deref(),
                    Some("SELECT" | "WITH")
                )
            {
                return Err(bad("query mode requires SELECT or WITH SQL"));
            }
            let values = sql_binds(args)?;
            let binds: Vec<(&str, &dyn ToSql)> = values
                .iter()
                .map(|(name, value)| (name.as_str(), value as &dyn ToSql))
                .collect();
            let conn = connect(config, timeout_secs, deadline)?;
            if mode == "query" {
                conn.execute("SET TRANSACTION READ ONLY", &[])?;
                conn.set_call_timeout(Some(remaining(deadline)?))?;
                let mut stmt = conn.statement(sql).lob_locator().build()?;
                let mut rows = stmt.query_named(&binds)?;
                let mut result = collect_rows(&conn, &mut rows, config, cancel, deadline)?;
                conn.rollback()?;
                result["mode"] = json!(mode);
                return Ok(result);
            }
            let operation = (|| -> Result<Value> {
                let stmt = conn.execute_named(sql, &binds)?;
                Ok(json!({"mode":mode,"affected_rows":stmt.row_count()?}))
            })();
            finish_mutation(&conn, operation, cancel, deadline)
        }
        "procedure" | "function" => {
            let fields = if mode == "function" {
                &["mode", "name", "args", "return_type"][..]
            } else {
                &["mode", "name", "args"][..]
            };
            validate_shape(args, &["mode", "name"], fields)?;
            let name = args["name"]
                .as_str()
                .ok_or_else(|| bad("name is required"))?;
            if !safe_name(name) {
                return Err(bad(
                    "name must be a one to three part unquoted Oracle identifier",
                ));
            }
            let call_args = call_args(args)?;
            let return_type = if mode == "function" {
                Some(
                    args["return_type"]
                        .as_str()
                        .ok_or_else(|| bad("function requires return_type"))?,
                )
            } else {
                None
            };
            if let Some(kind) = return_type {
                output_type(kind)?;
            }
            let placeholders = call_args
                .iter()
                .map(|arg| format!(":{}", arg.name))
                .collect::<Vec<_>>()
                .join(", ");
            let block = if mode == "function" {
                format!("BEGIN :mnemoarc_result := {name}({placeholders}); END;")
            } else {
                format!("BEGIN {name}({placeholders}); END;")
            };
            let conn = connect(config, timeout_secs, deadline)?;
            let operation = (|| -> Result<Value> {
                let mut stmt = conn.statement(&block).lob_locator().build()?;
                if let Some(kind) = return_type {
                    if kind == "cursor" {
                        stmt.bind("mnemoarc_result", &None::<RefCursor>)?;
                    } else {
                        stmt.bind("mnemoarc_result", &output_type(kind)?)?;
                    }
                }
                for arg in &call_args {
                    arg.input
                        .bind(&mut stmt, &arg.name, &arg.direction, &arg.kind)?;
                }
                conn.set_call_timeout(Some(remaining(deadline)?))?;
                stmt.execute(&[])?;
                let mut outs = Map::new();
                for arg in &call_args {
                    if arg.direction != "in" {
                        outs.insert(
                            arg.name.clone(),
                            read_output(
                                &stmt, &arg.name, &arg.kind, &conn, config, cancel, deadline,
                            )?,
                        );
                    }
                }
                let result = if let Some(kind) = return_type {
                    Some(read_output(
                        &stmt,
                        "mnemoarc_result",
                        kind,
                        &conn,
                        config,
                        cancel,
                        deadline,
                    )?)
                } else {
                    None
                };
                let mut implicit_results = Vec::new();
                for _ in 0..4 {
                    let Some(mut cursor) = stmt.implicit_result()? else {
                        break;
                    };
                    let mut rows = cursor.query()?;
                    implicit_results
                        .push(collect_rows(&conn, &mut rows, config, cancel, deadline)?);
                }
                let implicit_truncated =
                    implicit_results.len() == 4 && stmt.implicit_result()?.is_some();
                Ok(
                    json!({"mode":mode,"name":name,"out":outs,"result":result,"implicit_results":implicit_results,"implicit_truncated":implicit_truncated}),
                )
            })();
            finish_mutation(&conn, operation, cancel, deadline)
        }
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decimal_string_binds_preserve_all_digits() {
        for value in [
            "12345678901234567890.123456789012345678",
            "12345678901234567890123456789012345678",
            "-0.12345678901234567890123456789012345678",
            "1.2345678901234567890123456789012345678e-10",
        ] {
            let Input::Number(Some(bound)) = input(&json!(value), "number").unwrap() else {
                panic!("expected a numeric bind");
            };
            assert_eq!(bound, value);
        }
        for value in ["NaN", "inf", "1; DELETE", "1 2", "", "+1", "01", "1."] {
            assert!(input(&json!(value), "number").is_err(), "{value}");
        }
    }

    #[test]
    fn explicit_invalid_argument_types_do_not_select_defaults() {
        for field in ["direction", "type"] {
            for invalid in [json!(null), json!(false), json!(42), json!([]), json!({})] {
                let mut arg = json!({"name":"p_value","value":"text"});
                arg[field] = invalid;
                assert!(call_args(&json!({"args":[arg]})).is_err(), "{field}");
            }
        }
        let defaulted = call_args(&json!({"args":[{"name":"p_value","value":"text"}]})).unwrap();
        assert_eq!(defaulted[0].direction, "in");
        assert_eq!(defaulted[0].kind, "string");
    }

    #[test]
    fn sql_bind_names_must_be_unique_ignoring_case() {
        assert!(sql_binds(&json!({"params":{"item_id":1,"ITEM_ID":2}})).is_err());
        assert!(sql_binds(&json!({"params":{"item_id":1,"other_id":2}})).is_ok());
    }

    #[test]
    fn failed_rollback_preserves_both_causes_and_marks_outcome_uncertain() {
        let cause = anyhow::anyhow!("cancelled");
        let error = rollback_failure(cause, Err(anyhow::anyhow!("connection lost")));
        assert_eq!(
            error.to_string(),
            "database_rollback_uncertain: operation failed: cancelled; rollback failed: connection lost"
        );
        let error = rollback_failure(anyhow::anyhow!("statement failed"), Ok(()));
        assert_eq!(error.to_string(), "statement failed");
    }
}
