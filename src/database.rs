//! User-approved, named Oracle queries. The model never supplies SQL or changes enable flags.
mod free;
use anyhow::{Result, bail};
pub use free::execute_free;
use oracle::{Connection, sql_type::ToSql};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    pub enabled: bool,
    pub raw_query_enabled: bool,
    pub raw_statement_enabled: bool,
    pub procedure_enabled: bool,
    pub function_enabled: bool,
    pub host: String,
    pub port: u16,
    pub service: String,
    pub username: String,
    /// Name of an environment variable; the password itself is never serialized.
    pub password_env: String,
    pub max_rows: usize,
    pub queries: Vec<SavedQuery>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SavedQuery {
    pub id: String,
    pub description: String,
    pub sql: String,
    pub enabled: bool,
    pub params: Vec<QueryParam>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct QueryParam {
    pub name: String,
    pub description: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            raw_query_enabled: false,
            raw_statement_enabled: false,
            procedure_enabled: false,
            function_enabled: false,
            host: "localhost".into(),
            port: 1521,
            service: String::new(),
            username: String::new(),
            password_env: String::new(),
            max_rows: 100,
            queries: vec![],
        }
    }
}
fn identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn connect_component(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'.' | b'_' | b'-'))
}

impl DatabaseConfig {
    pub fn free_execution_enabled(&self) -> bool {
        self.enabled
            && (self.raw_query_enabled
                || self.raw_statement_enabled
                || self.procedure_enabled
                || self.function_enabled)
    }
    pub fn active_queries(&self) -> impl Iterator<Item = &SavedQuery> {
        self.queries.iter().filter(|q| self.enabled && q.enabled)
    }

    pub fn validate(&self) -> Result<()> {
        if self.max_rows == 0 || self.max_rows > 500 {
            bail!("database.max_rows must be between 1 and 500");
        }
        if self.port == 0 {
            bail!("database.port must be positive");
        }
        if self.enabled
            && (!connect_component(&self.host)
                || !connect_component(&self.service)
                || self.username.trim().is_empty()
                || !identifier(&self.password_env))
        {
            bail!(
                "database requires host, service, username and a valid password_env when enabled"
            );
        }
        let mut ids = BTreeSet::new();
        for q in &self.queries {
            if !identifier(&q.id) || !ids.insert(q.id.to_ascii_lowercase()) {
                bail!(
                    "database query IDs must be unique letters, numbers and underscores, starting with a letter"
                );
            }
            if q.description.trim().is_empty() {
                bail!("database query {} needs a description", q.id);
            }
            let sql = q.sql.trim_start();
            if !(sql.to_ascii_uppercase().starts_with("SELECT ")
                || sql.to_ascii_uppercase().starts_with("SELECT\n")
                || sql.to_ascii_uppercase().starts_with("WITH ")
                || sql.to_ascii_uppercase().starts_with("WITH\n"))
                || sql.contains(';')
            {
                bail!(
                    "database query {} must be a single SELECT or WITH query without a semicolon",
                    q.id
                );
            }
            let mut params = BTreeSet::new();
            for p in &q.params {
                if !identifier(&p.name)
                    || !params.insert(p.name.to_ascii_lowercase())
                    || p.description.trim().is_empty()
                {
                    bail!(
                        "database query {} has an invalid or duplicate parameter",
                        q.id
                    );
                }
            }
        }
        Ok(())
    }

    pub fn catalog(&self) -> Value {
        json!({"queries": self.active_queries().map(|q| json!({
            "id":q.id,"description":q.description,
            "params":q.params.iter().map(|p|json!({"name":p.name,"description":p.description,"value_type":"string or number; null accepted"})).collect::<Vec<_>>()
        })).collect::<Vec<_>>(),"usage":"Call db_query with action=run, id, and params keyed by parameter name. SQL is fixed by the user; all values are bound. Omit params or use {} when none are declared."})
    }
}

fn password(name: &str) -> Result<String> {
    if let Ok(value) = std::env::var(name) {
        return Ok(value);
    }
    for entry in dotenvy::from_path_iter(".env").into_iter().flatten() {
        let (key, value) = entry?;
        if key == name {
            return Ok(value);
        }
    }
    bail!("database_password_missing: environment variable {name} is not set")
}

fn remaining(deadline: Instant) -> Result<Duration> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        bail!("database_query_timeout");
    }
    Ok(left.min(Duration::from_secs(15)))
}

fn connect(config: &DatabaseConfig, timeout_secs: u64, deadline: Instant) -> Result<Connection> {
    let secret = password(&config.password_env)?;
    let connect_timeout = timeout_secs.clamp(1, 5);
    let connect = format!(
        "(DESCRIPTION=(CONNECT_TIMEOUT={connect_timeout})(RETRY_COUNT=0)(ADDRESS=(PROTOCOL=TCP)(HOST={})(PORT={}))(CONNECT_DATA=(SERVICE_NAME={})))",
        config.host, config.port, config.service
    );
    let conn = Connection::connect(&config.username, &secret, &connect)?;
    conn.set_call_timeout(Some(remaining(deadline)?))?;
    Ok(conn)
}

fn collect_rows(
    conn: &Connection,
    results: &mut oracle::ResultSet<'_, oracle::Row>,
    config: &DatabaseConfig,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Value> {
    let columns: Vec<_> = results
        .column_info()
        .iter()
        .map(|c| json!({"name":c.name(),"type":c.oracle_type().to_string()}))
        .collect();
    if columns.len() > 64 {
        bail!("database_result_too_wide: select fewer than 65 columns");
    }
    let mut rows = Vec::new();
    let mut truncated = false;
    let mut result_bytes = 0usize;
    loop {
        conn.set_call_timeout(Some(remaining(deadline)?))?;
        let Some(item) = results.next() else {
            break;
        };
        if cancel.is_cancelled() {
            bail!("cancelled");
        }
        if rows.len() == config.max_rows {
            truncated = true;
            break;
        }
        let row = item?;
        let mut cells = Vec::with_capacity(columns.len());
        for index in 0..columns.len() {
            let value: Option<String> = row.get(index)?;
            let value = value.map(|v| {
                let mut text = String::new();
                for ch in v.chars() {
                    if text.len() + ch.len_utf8() > 1024 {
                        text.push('…');
                        break;
                    }
                    text.push(ch);
                }
                text
            });
            cells.push(value);
        }
        let row_bytes: usize = cells
            .iter()
            .map(|v: &Option<String>| v.as_ref().map_or(4, String::len))
            .sum();
        if !rows.is_empty() && result_bytes.saturating_add(row_bytes) > 64 * 1024 {
            truncated = true;
            break;
        }
        result_bytes = result_bytes.saturating_add(row_bytes);
        rows.push(cells);
    }
    let row_count = rows.len();
    Ok(
        json!({"columns":columns,"rows":rows,"row_count":row_count,"truncated":truncated,"max_rows":config.max_rows}),
    )
}

pub fn execute(
    config: &DatabaseConfig,
    args: &Value,
    cancel: &CancellationToken,
    timeout_secs: u64,
) -> Result<Value> {
    if !config.enabled {
        bail!("database_disabled: enable database access manually in Settings");
    }
    if args["action"] == "list" {
        if args.get("id").is_some() || args.get("params").is_some() {
            bail!("invalid_database_query_arguments: list accepts only action");
        }
        return Ok(config.catalog());
    }
    if args["action"] != "run" {
        bail!("invalid_database_query_arguments: action must be list or run");
    }
    let id = args["id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("invalid_database_query_arguments: run requires id"))?;
    let query = config
        .active_queries()
        .find(|q| q.id == id)
        .ok_or_else(|| anyhow::anyhow!("database_query_disabled_or_unknown: {id}"))?;
    let empty = serde_json::Map::new();
    let supplied = match args.get("params") {
        None => &empty,
        Some(value) => value.as_object().ok_or_else(|| {
            anyhow::anyhow!("invalid_database_query_arguments: params must be an object")
        })?,
    };
    let names: BTreeSet<_> = query.params.iter().map(|p| p.name.as_str()).collect();
    if supplied.keys().any(|key| !names.contains(key.as_str()))
        || query.params.iter().any(|p| !supplied.contains_key(&p.name))
    {
        bail!(
            "invalid_database_query_arguments: parameters must match exactly: {}",
            query
                .params
                .iter()
                .map(|p| p.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    let values: Vec<Option<String>> = query
        .params
        .iter()
        .map(|p| match &supplied[&p.name] {
            Value::Null => Ok(None),
            Value::String(v) => Ok(Some(v.clone())),
            Value::Number(v) => Ok(Some(v.to_string())),
            _ => bail!(
                "invalid_database_query_arguments: parameter {} must be a string, number or null",
                p.name
            ),
        })
        .collect::<Result<_>>()?;
    if cancel.is_cancelled() {
        bail!("cancelled");
    }
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let conn = connect(config, timeout_secs, deadline)?;
    // Oracle enforces transaction read-only in addition to the SELECT-only query API.
    conn.execute("SET TRANSACTION READ ONLY", &[])?;
    conn.set_call_timeout(Some(remaining(deadline)?))?;
    let binds: Vec<(&str, &dyn ToSql)> = query
        .params
        .iter()
        .zip(&values)
        .map(|(p, v)| (p.name.as_str(), v as &dyn ToSql))
        .collect();
    let mut results = conn.query_named(&query.sql, &binds)?;
    let mut result = collect_rows(&conn, &mut results, config, cancel, deadline)?;
    conn.rollback()?;
    result["query_id"] = json!(id);
    Ok(result)
}
