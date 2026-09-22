use mnemoarc::{
    config::Config,
    database::{DatabaseConfig, QueryParam, SavedQuery},
    session::Session,
    tools::{self, ToolRegistry},
};
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn query(id: &str, enabled: bool, sql: &str) -> SavedQuery {
    SavedQuery {
        id: id.into(),
        description: format!("Read {id}"),
        sql: sql.into(),
        enabled,
        params: vec![],
    }
}

#[test]
fn database_is_user_gated_and_queries_are_individual() {
    let mut config = Config::default();
    config.database.queries = vec![
        query("allowed", true, "SELECT 1 FROM dual"),
        query("blocked", false, "SELECT 2 FROM dual"),
    ];
    let mut session = Session::new(Default::default(), config.clone());
    assert!(
        !ToolRegistry::definitions(&session)
            .iter()
            .any(|v| v["function"]["name"] == "db_query")
    );
    let hidden_catalog = tools::execute(&mut session, "tool_catalog", json!({"query":"db_query"})).unwrap();
    assert!(!hidden_catalog["tools"].as_array().unwrap().iter().any(|tool| tool["name"] == "db_query"));
    assert!(tools::execute(&mut session, "db_query", json!({"action":"list"})).is_err());
    config.database.enabled = true;
    config.database.service = "FREEPDB1".into();
    config.database.username = "SYSTEM".into();
    config.database.password_env = "MNEMOARC_TEST_DB_PASSWORD".into();
    session.config = config;
    let definition = ToolRegistry::definitions(&session)
        .into_iter()
        .find(|v| v["function"]["name"] == "db_query")
        .unwrap();
    assert_eq!(
        definition["function"]["parameters"]["properties"]["id"]["enum"],
        json!(["allowed"])
    );
    let catalog = tools::execute(&mut session, "db_query", json!({"action":"list"})).unwrap();
    assert_eq!(catalog["queries"].as_array().unwrap().len(), 1);
    assert!(
        tools::execute(
            &mut session,
            "db_query",
            json!({"action":"run","id":"blocked"})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "tool_select",
            json!({"action":"add","names":["db_query"]})
        )
        .is_err()
    );
    assert!(
        tools::execute(
            &mut session,
            "db_query",
            json!({"action":"run","id":"allowed","sql":"DELETE FROM dual"})
        )
        .is_err()
    );
}

#[test]
fn invalid_query_configuration_is_rejected() {
    let mut config = DatabaseConfig::default();
    config
        .queries
        .push(query("test", false, "DELETE FROM users"));
    assert!(config.validate().is_err());
    config.queries[0].sql = "SELECT 1 FROM dual".into();
    config
        .queries
        .push(query("TEST", false, "SELECT 2 FROM dual"));
    assert!(config.validate().is_err());
}

#[test]
fn example_configuration_keeps_database_disabled() {
    let config: Config = toml::from_str(include_str!("../config.example.toml")).unwrap();
    config.validate().unwrap();
    assert!(!config.database.enabled);
    assert!(config.database.queries.iter().all(|query| !query.enabled));
}

#[test]
fn oracle_docker_query_uses_binds_and_limits_rows() {
    if std::env::var("MNEMOARC_TEST_DB_PASSWORD").is_err() {
        return;
    }
    let mut config = DatabaseConfig {
        enabled: true,
        service: "FREEPDB1".into(),
        username: "SYSTEM".into(),
        password_env: "MNEMOARC_TEST_DB_PASSWORD".into(),
        max_rows: 2,
        ..Default::default()
    };
    let mut named = query("echo_value", true, "SELECT :value AS VALUE FROM dual");
    named.params.push(QueryParam {
        name: "value".into(),
        description: "Value to return".into(),
    });
    config.queries = vec![
        named,
        query(
            "three_rows",
            true,
            "SELECT level AS N FROM dual CONNECT BY level <= 3",
        ),
        query("date_and_number", true, "SELECT SYSDATE AS TODAY, 7 AS N FROM dual"),
    ];
    config.validate().unwrap();
    let result = mnemoarc::database::execute(
        &config,
        &json!({"action":"run","id":"echo_value","params":{"value":"x' OR 1=1 --"}}),
        &CancellationToken::new(),
        30,
    )
    .unwrap();
    assert_eq!(result["rows"], json!([["x' OR 1=1 --"]]));
    let bounded = mnemoarc::database::execute(
        &config,
        &json!({"action":"run","id":"three_rows"}),
        &CancellationToken::new(),
        30,
    )
    .unwrap();
    assert_eq!(bounded["row_count"], 2);
    assert_eq!(bounded["truncated"], true);
    let typed = mnemoarc::database::execute(
        &config,
        &json!({"action":"run","id":"date_and_number"}),
        &CancellationToken::new(),
        30,
    ).unwrap();
    assert_eq!(typed["rows"][0][1], "7");
    assert!(typed["rows"][0][0].as_str().is_some_and(|value| !value.is_empty()));
}
