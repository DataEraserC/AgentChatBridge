//! `acb-issue-service` CLI：serve / token-create / token-rotate / token-revoke /
//! backup / migrate。

use std::env;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::Arc;

use acb_issue_service::server::{ServerState, serve};
use acb_issue_service::{
    ALL_SCOPES, backup_database, create_token, now_ms, open_db, revoke_token, rotate_token,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("acb-issue-service: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    let command = args.next().unwrap_or_else(|| "serve".to_owned());
    let db = env::var("ACB_ISSUE_DB").unwrap_or_else(|_| "issues.db".to_owned());
    match command.as_str() {
        "serve" => {
            let listen =
                env::var("ACB_ISSUE_LISTEN").unwrap_or_else(|_| "127.0.0.1:7800".to_owned());
            let admin =
                env::var("ACB_ISSUE_ADMIN_PRINCIPAL").unwrap_or_else(|_| "admin".to_owned());
            let listener =
                TcpListener::bind(&listen).map_err(|error| format!("bind {listen}: {error}"))?;
            eprintln!("acb-issue-service listening on {listen}, db={db}, admin={admin}");
            serve(
                listener,
                Arc::new(ServerState {
                    db: PathBuf::from(db),
                    admin,
                }),
            );
            Ok(())
        }
        "migrate" => {
            let _conn = open_db(&db).map_err(|error| error.to_string())?;
            println!("migrated {db}");
            Ok(())
        }
        "token-create" => {
            let principal = required_arg(&mut args, "principal")?;
            let name = args.next().unwrap_or_else(|| "token".to_owned());
            let scopes = args
                .next()
                .map(|value| {
                    value
                        .split(',')
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| ALL_SCOPES.iter().map(|scope| (*scope).to_owned()).collect());
            let days = args
                .next()
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(90);
            let conn = open_db(&db).map_err(|error| error.to_string())?;
            let token = create_token(
                &conn,
                &principal,
                &name,
                &scopes,
                now_ms() + days * 24 * 3600 * 1000,
                now_ms(),
                None,
            )
            .map_err(|error| error.to_string())?;
            println!("{}", token.plaintext);
            Ok(())
        }
        "token-rotate" => {
            let id = required_arg(&mut args, "token-id")?;
            let conn = open_db(&db).map_err(|error| error.to_string())?;
            let token = rotate_token(&conn, &id, now_ms()).map_err(|error| error.to_string())?;
            println!("{}", token.plaintext);
            Ok(())
        }
        "token-revoke" => {
            let id = required_arg(&mut args, "token-id")?;
            let conn = open_db(&db).map_err(|error| error.to_string())?;
            revoke_token(&conn, &id, now_ms()).map_err(|error| error.to_string())?;
            println!("revoked {id}");
            Ok(())
        }
        "backup" => {
            let target = required_arg(&mut args, "target-path")?;
            let conn = open_db(&db).map_err(|error| error.to_string())?;
            backup_database(&conn, &target).map_err(|error| error.to_string())?;
            println!("backup written to {target}");
            Ok(())
        }
        other => Err(format!(
            "unknown command {other}; use serve|migrate|token-create|token-rotate|token-revoke|backup"
        )),
    }
}

fn required_arg(args: &mut impl Iterator<Item = String>, name: &str) -> Result<String, String> {
    args.next()
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("missing {name}"))
}
