use anyhow::{Result, ensure};
use mnemoarc::{
    config::Config,
    database::{DatabaseConfig, execute_free},
    session::Session,
    tools::{self, ToolRegistry},
};
use oracle::Connection;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn configured() -> DatabaseConfig {
    DatabaseConfig {
        enabled: true,
        service: "FREEPDB1".into(),
        username: "SYSTEM".into(),
        password_env: "MNEMOARC_TEST_DB_PASSWORD".into(),
        raw_query_enabled: true,
        raw_statement_enabled: true,
        procedure_enabled: true,
        function_enabled: true,
        ..Default::default()
    }
}

#[test]
fn free_execution_requires_manual_mode_switches() {
    let mut session = Session::new(Default::default(), Config::default());
    assert!(
        !ToolRegistry::definitions(&session)
            .iter()
            .any(|v| v["function"]["name"] == "db_execute")
    );
    assert!(
        tools::execute(
            &mut session,
            "db_execute",
            json!({"mode":"query","sql":"SELECT 1 FROM dual"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "tool_select",
            json!({"action":"add","names":["db_execute"]})
        )
        .is_err()
    );
    session.config.database = configured();
    session.config.database.raw_statement_enabled = false;
    session.config.database.procedure_enabled = false;
    session.config.database.function_enabled = false;
    let spec = ToolRegistry::definitions(&session)
        .into_iter()
        .find(|v| v["function"]["name"] == "db_execute")
        .unwrap();
    assert_eq!(
        spec["function"]["parameters"]["properties"]["mode"]["enum"],
        json!(["query"])
    );
    assert!(
        tools::execute(
            &mut session,
            "db_execute",
            json!({"mode":"statement","sql":"DELETE FROM anything"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "db_execute",
            json!({"mode":"procedure","name":"DBMS_OUTPUT.ENABLE"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "db_execute",
            json!({"mode":"function","name":"UPPER","return_type":"string"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "db_execute",
            json!({"mode":"query","sql":"DELETE FROM anything"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "db_execute",
            json!({"mode":"query","sql":"SELECT 1 FROM dual","name":"unexpected"})
        )
        .is_err()
    );
}

#[test]
fn oracle_free_execution_round_trips_sql_procedure_function_and_cursor() -> Result<()> {
    let Ok(password) = std::env::var("MNEMOARC_TEST_DB_PASSWORD") else {
        return Ok(());
    };
    let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_ascii_uppercase();
    let table = format!("MA_TEST_{suffix}");
    let procedure = format!("MA_PROC_{suffix}");
    let function = format!("MA_FUNC_{suffix}");
    let cursor_function = format!("MA_FCUR_{suffix}");
    let failing_procedure = format!("MA_FAIL_{suffix}");
    let conn = Connection::connect("SYSTEM", &password, "//localhost:1521/FREEPDB1")?;
    let config = configured();
    let cancel = CancellationToken::new();
    let create = execute_free(
        &config,
        &json!({"mode":"statement","sql":format!("CREATE TABLE {table} (NOTE VARCHAR2(100))")}),
        &cancel,
        30,
    )?;
    ensure!(create["committed"] == true, "create result: {create}");
    let test = (|| -> Result<()> {
        conn.execute(&format!("CREATE OR REPLACE PROCEDURE {procedure}(p_in IN VARCHAR2, p_out OUT VARCHAR2, p_cursor OUT SYS_REFCURSOR, p_num IN OUT NUMBER, p_flag OUT BOOLEAN) AS BEGIN INSERT INTO {table}(NOTE) VALUES (p_in); p_out := UPPER(p_in); OPEN p_cursor FOR SELECT NOTE FROM {table} ORDER BY NOTE; p_num := p_num + 1; p_flag := TRUE; END;"), &[])?;
        conn.execute(&format!("CREATE OR REPLACE FUNCTION {function}(p_in IN NUMBER) RETURN NUMBER AS BEGIN RETURN p_in + 1; END;"), &[])?;
        conn.execute(&format!("CREATE OR REPLACE FUNCTION {cursor_function} RETURN SYS_REFCURSOR AS c SYS_REFCURSOR; BEGIN OPEN c FOR SELECT NOTE FROM {table} ORDER BY NOTE; RETURN c; END;"), &[])?;
        conn.execute(&format!("CREATE OR REPLACE PROCEDURE {failing_procedure} AS BEGIN INSERT INTO {table}(NOTE) VALUES ('uncommitted'); RAISE_APPLICATION_ERROR(-20000, 'expected failure'); END;"), &[])?;
        // Oracle read-only snapshots can reject a table changed in the same second.
        std::thread::sleep(std::time::Duration::from_secs(2));
        let write = execute_free(
            &config,
            &json!({"mode":"statement","sql":format!("INSERT INTO {table}(NOTE) VALUES (:note)"),"params":{"note":"first"}}),
            &cancel,
            30,
        )?;
        ensure!(
            write["committed"] == true && write["affected_rows"] == 1,
            "write result: {write}"
        );
        let read = execute_free(
            &config,
            &json!({"mode":"query","sql":format!("SELECT NOTE FROM {table} WHERE NOTE = :note"),"params":{"note":"first"}}),
            &cancel,
            30,
        )?;
        ensure!(read["rows"] == json!([["first"]]), "read result: {read}");
        let call = execute_free(
            &config,
            &json!({"mode":"procedure","name":procedure,"args":[
                {"name":"p_in","direction":"in","type":"string","value":"second"},
                {"name":"p_out","direction":"out","type":"string"},
                {"name":"p_cursor","direction":"out","type":"cursor"},
                {"name":"p_num","direction":"inout","type":"number","value":5},
                {"name":"p_flag","direction":"out","type":"boolean"}
            ]}),
            &cancel,
            30,
        )?;
        ensure!(call["out"]["p_out"] == "SECOND", "procedure OUT: {call}");
        ensure!(call["out"]["p_num"] == "6", "procedure INOUT: {call}");
        ensure!(call["out"]["p_flag"] == true, "procedure BOOLEAN: {call}");
        ensure!(
            call["out"]["p_cursor"]["rows"] == json!([["first"], ["second"]]),
            "procedure cursor: {call}"
        );
        let value = execute_free(
            &config,
            &json!({"mode":"function","name":function,"return_type":"number","args":[{"name":"p_in","type":"number","value":41}]}),
            &cancel,
            30,
        )?;
        ensure!(value["result"] == "42", "function result: {value}");
        let cursor_value = execute_free(
            &config,
            &json!({"mode":"function","name":cursor_function,"return_type":"cursor"}),
            &cancel,
            30,
        )?;
        ensure!(
            cursor_value["result"]["rows"] == json!([["first"], ["second"]]),
            "function cursor: {cursor_value}"
        );
        ensure!(
            execute_free(
                &config,
                &json!({"mode":"procedure","name":failing_procedure}),
                &cancel,
                30
            )
            .is_err(),
            "failing procedure should return an error"
        );
        let committed = execute_free(
            &config,
            &json!({"mode":"query","sql":format!("SELECT COUNT(*) AS N FROM {table}")}),
            &cancel,
            30,
        )?;
        ensure!(
            committed["rows"] == json!([["2"]]),
            "commit check: {committed}"
        );
        Ok(())
    })();
    let _ = conn.execute(&format!("DROP FUNCTION {function}"), &[]);
    let _ = conn.execute(&format!("DROP FUNCTION {cursor_function}"), &[]);
    let _ = conn.execute(&format!("DROP PROCEDURE {procedure}"), &[]);
    let _ = conn.execute(&format!("DROP PROCEDURE {failing_procedure}"), &[]);
    let _ = conn.execute(&format!("DROP TABLE {table} PURGE"), &[]);
    test
}
