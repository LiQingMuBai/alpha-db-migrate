use anyhow::{Context, Result, bail};
use dotenvy::dotenv;
use mysql::{Error as MySqlDriverError, MySqlError, Opts, Pool, PooledConn, prelude::Queryable};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Instant;
use url::Url;

#[derive(Clone, Debug)]
struct AppConfig {
    mysql_dsn: String,
    sql_dir: PathBuf,
    charset: String,
    disable_foreign_key_checks: bool,
}

#[derive(Debug)]
enum ImportTaskResult {
    Imported,
    Skipped {
        database_name: String,
        file: PathBuf,
        reason: &'static str,
    },
    Failed {
        database_name: String,
        err: anyhow::Error,
    },
}

fn main() -> Result<()> {
    dotenv().ok();
    let config = AppConfig::from_env()?;
    let files = discover_sql_files(&config.sql_dir)?;

    if files.is_empty() {
        bail!(
            "No .sql files found in directory {}",
            config.sql_dir.display()
        );
    }

    println!(
        "Starting import. Found {} SQL files in {}",
        files.len(),
        config.sql_dir.display()
    );
    println!("Concurrent mode enabled: one thread per SQL import task");

    let started_at = Instant::now();
    let (imported_count, skipped_count) = run_import_tasks(&config, files)?;

    println!(
        "Import completed in {:.2?}. Imported: {}, Skipped: {}",
        started_at.elapsed(),
        imported_count,
        skipped_count
    );
    Ok(())
}

impl AppConfig {
    fn from_env() -> Result<Self> {
        Ok(Self {
            mysql_dsn: read_required_env("MYSQL_DSN")?,
            sql_dir: env::var("SQL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./sql")),
            charset: env::var("MYSQL_CHARSET").unwrap_or_else(|_| "utf8mb4".to_string()),
            disable_foreign_key_checks: env::var("DISABLE_FOREIGN_KEY_CHECKS")
                .ok()
                .map(|value| parse_bool_env("DISABLE_FOREIGN_KEY_CHECKS", &value))
                .transpose()?
                .unwrap_or(true),
        })
    }
}

fn run_import_tasks(config: &AppConfig, files: Vec<PathBuf>) -> Result<(usize, usize)> {
    let tasks = files
        .into_iter()
        .map(|file| {
            let database_name = database_name_from_file(&file)?;
            Ok((file, database_name))
        })
        .collect::<Result<Vec<_>>>()?;

    let mut handles = Vec::with_capacity(tasks.len());

    for (file, database_name) in tasks {
        let config = config.clone();
        handles.push(thread::spawn(move || {
            match import_one_file(&config, &file, &database_name) {
                Ok(()) => ImportTaskResult::Imported,
                Err(err) => match is_skippable_database_error(&err) {
                    Some(reason) => ImportTaskResult::Skipped {
                        database_name,
                        file,
                        reason,
                    },
                    None => ImportTaskResult::Failed { database_name, err },
                },
            }
        }));
    }

    let mut imported_count = 0usize;
    let mut skipped_count = 0usize;
    let mut first_fatal_error: Option<anyhow::Error> = None;

    for handle in handles {
        match handle.join() {
            Ok(ImportTaskResult::Imported) => imported_count += 1,
            Ok(ImportTaskResult::Skipped {
                database_name,
                file,
                reason,
            }) => {
                skipped_count += 1;
                eprintln!(
                    "[{database_name}] Notice: {}. Skipped file {}",
                    reason,
                    file.display()
                );
            }
            Ok(ImportTaskResult::Failed { database_name, err }) => {
                if first_fatal_error.is_none() {
                    first_fatal_error = Some(
                        err.context(format!("Import task failed for database {}", database_name)),
                    );
                }
            }
            Err(_) => {
                if first_fatal_error.is_none() {
                    first_fatal_error = Some(anyhow::anyhow!(
                        "An import worker thread exited unexpectedly"
                    ));
                }
            }
        }
    }

    if let Some(err) = first_fatal_error {
        return Err(err);
    }

    Ok((imported_count, skipped_count))
}

fn read_required_env(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("Missing environment variable: {key}. Please check .env"))
}

fn parse_bool_env(key: &str, value: &str) -> Result<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => bail!("Invalid value for environment variable {key}: {value}"),
    }
}

fn discover_sql_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();

    for entry in fs::read_dir(dir)
        .with_context(|| format!("Failed to read SQL directory: {}", dir.display()))?
    {
        let entry = entry?;
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("sql"))
        {
            files.push(path);
        }
    }

    files.sort();
    Ok(files)
}

fn database_name_from_file(path: &Path) -> Result<String> {
    let database_name = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .with_context(|| {
            format!(
                "Failed to parse database name from file name: {}",
                path.display()
            )
        })?;

    Ok(database_name)
}

fn import_one_file(config: &AppConfig, file: &Path, database_name: &str) -> Result<()> {
    let content = fs::read_to_string(file)
        .with_context(|| format!("Failed to read SQL file: {}", file.display()))?;
    let statements = split_sql_statements(&content)
        .with_context(|| format!("Failed to parse SQL file: {}", file.display()))?;

    println!(
        "\n[{database_name}] Starting import. File: {}, Statements: {}",
        file.display(),
        statements.len()
    );

    let mut conn = connect_to_database(config, database_name)
        .with_context(|| format!("Failed to connect to database: {}", database_name))?;

    if config.disable_foreign_key_checks {
        conn.query_drop("SET FOREIGN_KEY_CHECKS = 0")
            .with_context(|| format!("Failed to disable foreign key checks: {}", database_name))?;
    }

    reset_database_objects(&mut conn, database_name)?;

    let started_at = Instant::now();

    for (index, statement) in statements.iter().enumerate() {
        conn.query_drop(statement.as_str()).with_context(|| {
            format!(
                "Execution failed. Database: {}, Statement #{}, Preview: {}",
                database_name,
                index + 1,
                preview_sql(statement)
            )
        })?;
    }

    if config.disable_foreign_key_checks {
        conn.query_drop("SET FOREIGN_KEY_CHECKS = 1")
            .with_context(|| {
                format!("Failed to re-enable foreign key checks: {}", database_name)
            })?;
    }

    println!(
        "[{database_name}] Import finished in {:.2?}",
        started_at.elapsed()
    );
    Ok(())
}

fn reset_database_objects(conn: &mut PooledConn, database_name: &str) -> Result<()> {
    let objects: Vec<(String, String)> = conn
        .query(
            r#"
            SELECT TABLE_NAME, TABLE_TYPE
            FROM information_schema.TABLES
            WHERE TABLE_SCHEMA = DATABASE()
            ORDER BY TABLE_TYPE, TABLE_NAME
            "#,
        )
        .with_context(|| format!("Failed to read database objects: {}", database_name))?;

    if objects.is_empty() {
        println!("[{database_name}] Reset phase: no existing tables or views found");
        return Ok(());
    }

    let mut view_count = 0usize;
    let mut table_count = 0usize;

    for (object_name, object_type) in objects {
        let escaped_name = escape_identifier(&object_name);
        let sql = if object_type.eq_ignore_ascii_case("VIEW") {
            view_count += 1;
            format!("DROP VIEW IF EXISTS `{escaped_name}`")
        } else {
            table_count += 1;
            format!("DROP TABLE IF EXISTS `{escaped_name}`")
        };

        conn.query_drop(sql).with_context(|| {
            format!(
                "Failed to reset database. Database: {}, Object: {}, Type: {}",
                database_name, object_name, object_type
            )
        })?;
    }

    println!(
        "[{database_name}] Reset phase finished. Dropped {} tables and {} views",
        table_count, view_count
    );
    Ok(())
}

fn connect_to_database(config: &AppConfig, database_name: &str) -> Result<PooledConn> {
    let dsn = build_database_dsn(&config.mysql_dsn, database_name)?;
    let opts = Opts::from_url(&dsn)
        .with_context(|| format!("Invalid MYSQL_DSN format: {}", mask_dsn(&dsn)))?;
    let pool = Pool::new(opts)?;
    let mut conn = pool.get_conn()?;
    conn.query_drop(format!("SET NAMES {}", config.charset))?;
    Ok(conn)
}

fn build_database_dsn(base_dsn: &str, database_name: &str) -> Result<String> {
    let mut url = Url::parse(base_dsn)
        .with_context(|| format!("MYSQL_DSN is not a valid URL: {}", mask_dsn(base_dsn)))?;
    url.set_path(&format!("/{}", database_name));
    Ok(url.to_string())
}

fn mask_dsn(dsn: &str) -> String {
    match Url::parse(dsn) {
        Ok(mut url) => {
            if url.password().is_some() {
                let _ = url.set_password(Some("******"));
            }
            url.to_string()
        }
        Err(_) => "***".to_string(),
    }
}

fn escape_identifier(name: &str) -> String {
    name.replace('`', "``")
}

fn is_skippable_database_error(err: &anyhow::Error) -> Option<&'static str> {
    mysql_error_code(err).and_then(skip_reason_for_mysql_error)
}

fn mysql_error_code(err: &anyhow::Error) -> Option<u16> {
    err.chain().find_map(|cause| {
        cause
            .downcast_ref::<MySqlDriverError>()
            .and_then(|mysql_err| match mysql_err {
                MySqlDriverError::MySqlError(server_err) => Some(server_err.code),
                _ => None,
            })
            .or_else(|| {
                cause
                    .downcast_ref::<MySqlError>()
                    .map(|server_err| server_err.code)
            })
    })
}

fn skip_reason_for_mysql_error(code: u16) -> Option<&'static str> {
    match code {
        1049 => Some("database does not exist"),
        1044 | 1045 => Some("access denied for this database"),
        1142 | 1227 => Some("insufficient privileges to run the import"),
        _ => None,
    }
}

fn preview_sql(statement: &str) -> String {
    const LIMIT: usize = 120;
    let flat = statement.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() <= LIMIT {
        flat
    } else {
        let preview: String = flat.chars().take(LIMIT).collect();
        format!("{preview}...")
    }
}

fn split_sql_statements(input: &str) -> Result<Vec<String>> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut delimiter = ";".to_string();

    let mut in_single_quote = false;
    let mut in_double_quote = false;
    let mut in_backtick = false;
    let mut in_line_comment = false;
    let mut in_block_comment = false;
    let mut escaped = false;

    for line in input.lines() {
        let trimmed = line.trim_start();
        if !in_single_quote
            && !in_double_quote
            && !in_backtick
            && !in_line_comment
            && !in_block_comment
            && trimmed
                .get(..10)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("DELIMITER "))
        {
            delimiter = trimmed["DELIMITER ".len()..].trim().to_string();
            if delimiter.is_empty() {
                bail!("Encountered an empty DELIMITER declaration");
            }
            continue;
        }

        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let ch = chars[i];
            let next = chars.get(i + 1).copied();

            if in_line_comment {
                current.push(ch);
                i += 1;
                continue;
            }

            if in_block_comment {
                current.push(ch);
                if ch == '*' && next == Some('/') {
                    current.push('/');
                    in_block_comment = false;
                    i += 2;
                    continue;
                }
                i += 1;
                continue;
            }

            if in_single_quote {
                current.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '\'' {
                    in_single_quote = false;
                }
                i += 1;
                continue;
            }

            if in_double_quote {
                current.push(ch);
                if escaped {
                    escaped = false;
                } else if ch == '\\' {
                    escaped = true;
                } else if ch == '"' {
                    in_double_quote = false;
                }
                i += 1;
                continue;
            }

            if in_backtick {
                current.push(ch);
                if ch == '`' {
                    in_backtick = false;
                }
                i += 1;
                continue;
            }

            if ch == '-' && next == Some('-') && chars.get(i + 2).is_some_and(|c| c.is_whitespace())
            {
                current.push(ch);
                current.push('-');
                in_line_comment = true;
                i += 2;
                continue;
            }

            if ch == '#' {
                current.push(ch);
                in_line_comment = true;
                i += 1;
                continue;
            }

            if ch == '/' && next == Some('*') {
                current.push(ch);
                current.push('*');
                in_block_comment = true;
                i += 2;
                continue;
            }

            if ch == '\'' {
                current.push(ch);
                in_single_quote = true;
                i += 1;
                continue;
            }

            if ch == '"' {
                current.push(ch);
                in_double_quote = true;
                i += 1;
                continue;
            }

            if ch == '`' {
                current.push(ch);
                in_backtick = true;
                i += 1;
                continue;
            }

            current.push(ch);

            if current.ends_with(&delimiter) {
                let statement = current[..current.len() - delimiter.len()]
                    .trim()
                    .to_string();
                if !statement.is_empty() {
                    statements.push(statement);
                }
                current.clear();
            }

            i += 1;
        }

        if in_line_comment {
            in_line_comment = false;
        }

        current.push('\n');
    }

    let tail = current.trim();
    if !tail.is_empty() {
        statements.push(tail.to_string());
    }

    Ok(statements)
}

#[cfg(test)]
mod tests {
    use super::{
        build_database_dsn, escape_identifier, is_skippable_database_error, split_sql_statements,
    };
    use anyhow::anyhow;
    use mysql::{Error as MySqlDriverError, MySqlError};

    #[test]
    fn splits_basic_statements() {
        let sql = "CREATE TABLE t(id INT);\nINSERT INTO t VALUES (1);";
        let statements = split_sql_statements(sql).unwrap();
        assert_eq!(statements.len(), 2);
    }

    #[test]
    fn ignores_semicolon_inside_string() {
        let sql = "INSERT INTO t VALUES ('a;b');\nINSERT INTO t VALUES ('c');";
        let statements = split_sql_statements(sql).unwrap();
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("'a;b'"));
    }

    #[test]
    fn supports_delimiter_blocks() {
        let sql = r#"
DELIMITER $$
CREATE PROCEDURE demo()
BEGIN
    SELECT 1;
    SELECT 2;
END$$
DELIMITER ;
INSERT INTO t VALUES (1);
"#;
        let statements = split_sql_statements(sql).unwrap();
        assert_eq!(statements.len(), 2);
        assert!(statements[0].contains("CREATE PROCEDURE"));
        assert!(statements[1].starts_with("INSERT INTO"));
    }

    #[test]
    fn keeps_comments_without_breaking_split() {
        let sql = r#"
-- comment;
CREATE TABLE t(id INT);
# comment;
INSERT INTO t VALUES (1);
"#;
        let statements = split_sql_statements(sql).unwrap();
        assert_eq!(statements.len(), 2);
    }

    #[test]
    fn builds_database_dsn_from_base_url() {
        let dsn = build_database_dsn("mysql://user:pass@127.0.0.1:3306", "gtt").unwrap();
        assert_eq!(dsn, "mysql://user:pass@127.0.0.1:3306/gtt");
    }

    #[test]
    fn replaces_database_name_inside_dsn() {
        let dsn = build_database_dsn(
            "mysql://user:pass@127.0.0.1:3306/old_db?ssl-mode=DISABLED",
            "gtt",
        )
        .unwrap();
        assert!(dsn.starts_with("mysql://user:pass@127.0.0.1:3306/gtt?"));
    }

    #[test]
    fn detects_missing_database_error() {
        let err = anyhow!(MySqlDriverError::MySqlError(MySqlError {
            state: "42000".to_string(),
            message: "Unknown database 'gtt'".to_string(),
            code: 1049,
        }));
        assert_eq!(
            is_skippable_database_error(&err),
            Some("database does not exist")
        );
    }

    #[test]
    fn ignores_other_mysql_errors() {
        let err = anyhow!(MySqlDriverError::MySqlError(MySqlError {
            state: "42000".to_string(),
            message: "Syntax error".to_string(),
            code: 1064,
        }));
        assert_eq!(is_skippable_database_error(&err), None);
    }

    #[test]
    fn detects_permission_denied_error() {
        let err = anyhow!(MySqlDriverError::MySqlError(MySqlError {
            state: "42000".to_string(),
            message: "Access denied".to_string(),
            code: 1044,
        }));
        assert_eq!(
            is_skippable_database_error(&err),
            Some("access denied for this database")
        );
    }

    #[test]
    fn detects_privilege_error() {
        let err = anyhow!(MySqlDriverError::MySqlError(MySqlError {
            state: "42000".to_string(),
            message: "Command denied".to_string(),
            code: 1142,
        }));
        assert_eq!(
            is_skippable_database_error(&err),
            Some("insufficient privileges to run the import")
        );
    }

    #[test]
    fn escapes_backticks_in_identifier() {
        assert_eq!(escape_identifier("odd`name"), "odd``name");
    }
}
