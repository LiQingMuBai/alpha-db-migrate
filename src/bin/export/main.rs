use anyhow::{Context, Result, bail};
use dotenvy::dotenv;
use std::collections::HashSet;
use std::env;
use std::error::Error;
use std::fmt;
use std::fs::{self, File};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;
use url::Url;

#[derive(Clone, Debug)]
struct ExportConfig {
    export_mysql_dsn: String,
    export_databases: Vec<String>,
    ignored_databases: HashSet<String>,
    export_sql_dir: PathBuf,
    charset: String,
    mysqldump_bin: String,
}

#[derive(Debug)]
struct MysqlConnectionConfig {
    host: String,
    port: u16,
    user: String,
    password: Option<String>,
}

#[derive(Debug)]
struct DumpCommandError {
    stderr: String,
    exit_code: Option<i32>,
}

impl fmt::Display for DumpCommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.exit_code {
            Some(code) => write!(
                f,
                "mysqldump failed with exit code {}: {}",
                code, self.stderr
            ),
            None => write!(f, "mysqldump failed: {}", self.stderr),
        }
    }
}

impl Error for DumpCommandError {}

fn main() -> Result<()> {
    dotenv().ok();
    let config = ExportConfig::from_env()?;
    ensure_mysqldump_available()?;
    fs::create_dir_all(&config.export_sql_dir).with_context(|| {
        format!(
            "Failed to create export directory {}",
            config.export_sql_dir.display()
        )
    })?;

    println!(
        "Starting export. Requested databases: {}, output directory: {}",
        config.export_databases.len(),
        config.export_sql_dir.display()
    );

    let started_at = Instant::now();
    let (exported_count, skipped_count) = run_export_tasks(&config)?;

    println!(
        "Export completed in {:.2?}. Exported: {}, Skipped: {}",
        started_at.elapsed(),
        exported_count,
        skipped_count
    );
    Ok(())
}

impl ExportConfig {
    fn from_env() -> Result<Self> {
        let export_databases = parse_database_list_required("EXPORT_DATABASES")?;

        Ok(Self {
            export_mysql_dsn: read_optional_env("EXPORT_MYSQL_DSN")?
                .or_else(|| read_optional_env("MYSQL_DSN").ok().flatten())
                .with_context(|| {
                    "Missing environment variable: EXPORT_MYSQL_DSN (or MYSQL_DSN fallback). Please check .env"
                })?,
            export_databases,
            ignored_databases: env::var("IGNORE_DATABASES")
                .map(|value| parse_database_set(&value))
                .unwrap_or_default(),
            export_sql_dir: env::var("EXPORT_SQL_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./export_sql")),
            charset: env::var("MYSQL_CHARSET").unwrap_or_else(|_| "utf8mb4".to_string()),
            mysqldump_bin: resolve_mysqldump_bin()?,
        })
    }
}

fn run_export_tasks(config: &ExportConfig) -> Result<(usize, usize)> {
    let mut exported_count = 0usize;
    let mut skipped_count = 0usize;

    for database_name in &config.export_databases {
        if should_ignore_database(&config.ignored_databases, database_name) {
            skipped_count += 1;
            eprintln!("[{database_name}] Notice: ignored by configuration. Skipped export.");
            continue;
        }

        match export_one_database(config, database_name) {
            Ok(()) => exported_count += 1,
            Err(err) => match is_skippable_dump_error(&err) {
                Some(reason) => {
                    skipped_count += 1;
                    eprintln!("[{database_name}] Notice: {}. Skipped export.", reason);
                }
                None => {
                    return Err(
                        err.context(format!("Export task failed for database {}", database_name))
                    );
                }
            },
        }
    }

    Ok((exported_count, skipped_count))
}

fn export_one_database(config: &ExportConfig, database_name: &str) -> Result<()> {
    let connection = parse_mysql_dsn(&config.export_mysql_dsn)?;
    let output_path = config.export_sql_dir.join(format!("{database_name}.sql"));
    let output_file = File::create(&output_path)
        .with_context(|| format!("Failed to create export file {}", output_path.display()))?;

    println!(
        "[{database_name}] Starting export. Output file: {}",
        output_path.display()
    );

    let started_at = Instant::now();
    let mut command = Command::new(&config.mysqldump_bin);
    command
        .arg(format!("--host={}", connection.host))
        .arg(format!("--port={}", connection.port))
        .arg(format!("--user={}", connection.user))
        .arg(format!("--default-character-set={}", config.charset))
        .arg("--single-transaction")
        .arg("--routines")
        .arg("--events")
        .arg("--triggers")
        .arg("--set-gtid-purged=OFF")
        .arg("--databases")
        .arg(database_name)
        .stdout(Stdio::from(output_file))
        .stderr(Stdio::piped());

    if let Some(password) = connection.password {
        command.env("MYSQL_PWD", password);
    }

    let output = command
        .output()
        .with_context(|| format!("Failed to execute mysqldump for database {}", database_name))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let _ = fs::remove_file(&output_path);
        return Err(DumpCommandError {
            stderr,
            exit_code: output.status.code(),
        }
        .into());
    }

    println!(
        "[{database_name}] Export finished in {:.2?}. File: {}",
        started_at.elapsed(),
        output_path.display()
    );
    Ok(())
}

fn ensure_mysqldump_available() -> Result<()> {
    let dump_bin = resolve_mysqldump_bin()?;
    let output = Command::new(&dump_bin)
        .arg("--version")
        .output()
        .with_context(|| format!("Failed to launch dump tool: {}", dump_bin))?;

    if !output.status.success() {
        bail!(
            "Dump tool is installed but unavailable for execution: {}",
            dump_bin
        );
    }

    Ok(())
}

fn resolve_mysqldump_bin() -> Result<String> {
    if let Ok(configured) = env::var("MYSQLDUMP_BIN") {
        let trimmed = configured.trim();
        if trimmed.is_empty() {
            bail!("Environment variable MYSQLDUMP_BIN is empty");
        }
        return Ok(trimmed.to_string());
    }

    for candidate in dump_command_candidates() {
        if Command::new(candidate)
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
        {
            return Ok(candidate.to_string());
        }
    }

    bail!(
        "Failed to find a dump tool. Set MYSQLDUMP_BIN in .env, for example /opt/homebrew/opt/mysql-client/bin/mysqldump"
    )
}

fn dump_command_candidates() -> &'static [&'static str] {
    &[
        "mysqldump",
        "mariadb-dump",
        "/opt/homebrew/bin/mysqldump",
        "/opt/homebrew/bin/mariadb-dump",
        "/opt/homebrew/opt/mysql-client/bin/mysqldump",
        "/usr/local/bin/mysqldump",
        "/usr/local/mysql/bin/mysqldump",
    ]
}

fn parse_mysql_dsn(dsn: &str) -> Result<MysqlConnectionConfig> {
    let url = Url::parse(dsn)
        .with_context(|| format!("MYSQL_DSN is not a valid URL: {}", mask_dsn(dsn)))?;
    let host = url
        .host_str()
        .map(str::to_string)
        .with_context(|| format!("MYSQL_DSN is missing a host: {}", mask_dsn(dsn)))?;
    let user = url.username().trim().to_string();

    if user.is_empty() {
        bail!("MYSQL_DSN is missing a username: {}", mask_dsn(dsn));
    }

    Ok(MysqlConnectionConfig {
        host,
        port: url.port().unwrap_or(3306),
        user,
        password: url.password().map(str::to_string),
    })
}

fn read_required_env(key: &str) -> Result<String> {
    env::var(key).with_context(|| format!("Missing environment variable: {key}. Please check .env"))
}

fn read_optional_env(key: &str) -> Result<Option<String>> {
    match env::var(key) {
        Ok(value) => {
            let trimmed = value.trim().to_string();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed))
            }
        }
        Err(env::VarError::NotPresent) => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Failed to read environment variable: {key}")),
    }
}

fn parse_database_list_required(key: &str) -> Result<Vec<String>> {
    let value = read_required_env(key)?;
    let databases = parse_database_list(&value);

    if databases.is_empty() {
        bail!("Environment variable {key} must contain at least one database name");
    }

    Ok(databases)
}

fn parse_database_list(value: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut databases = Vec::new();

    for name in value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        let normalized = normalize_database_name(name);
        if seen.insert(normalized) {
            databases.push(name.trim().to_string());
        }
    }

    databases
}

fn parse_database_set(value: &str) -> HashSet<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(normalize_database_name)
        .collect()
}

fn normalize_database_name(database_name: &str) -> String {
    database_name.trim().to_ascii_lowercase()
}

fn should_ignore_database(ignored_databases: &HashSet<String>, database_name: &str) -> bool {
    ignored_databases.contains(&normalize_database_name(database_name))
}

fn is_skippable_dump_error(err: &anyhow::Error) -> Option<&'static str> {
    err.chain()
        .find_map(|cause| cause.downcast_ref::<DumpCommandError>())
        .and_then(|dump_err| skip_reason_for_dump_stderr(&dump_err.stderr))
}

fn skip_reason_for_dump_stderr(stderr: &str) -> Option<&'static str> {
    let normalized = stderr.to_ascii_lowercase();

    if normalized.contains("unknown database") || normalized.contains("error: 1049") {
        Some("database does not exist")
    } else if normalized.contains("access denied") {
        Some("access denied for this database")
    } else if normalized.contains("command denied")
        || normalized.contains("error: 1142")
        || normalized.contains("error: 1227")
    {
        Some("insufficient privileges to run the export")
    } else {
        None
    }
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

#[cfg(test)]
mod tests {
    use super::{
        dump_command_candidates, parse_database_list, parse_database_set, parse_mysql_dsn,
        should_ignore_database, skip_reason_for_dump_stderr,
    };

    #[test]
    fn parses_export_database_list_without_duplicates() {
        let databases = parse_database_list("gtt, order, gtt, billing");
        assert_eq!(databases, vec!["gtt", "order", "billing"]);
    }

    #[test]
    fn parses_mysql_dsn_for_mysqldump() {
        let config = parse_mysql_dsn("mysql://user:pass@127.0.0.1:3307/demo").unwrap();
        assert_eq!(config.host, "127.0.0.1");
        assert_eq!(config.port, 3307);
        assert_eq!(config.user, "user");
        assert_eq!(config.password.as_deref(), Some("pass"));
    }

    #[test]
    fn matches_ignored_database_case_insensitively() {
        let ignored = parse_database_set("gtt,order");
        assert!(should_ignore_database(&ignored, "GTT"));
        assert!(should_ignore_database(&ignored, "order"));
        assert!(!should_ignore_database(&ignored, "billing"));
    }

    #[test]
    fn detects_skippable_dump_errors() {
        assert_eq!(
            skip_reason_for_dump_stderr("mysqldump: Got error: 1049: Unknown database 'gtt'"),
            Some("database does not exist")
        );
        assert_eq!(
            skip_reason_for_dump_stderr("mysqldump: Got error: 1044: Access denied"),
            Some("access denied for this database")
        );
        assert_eq!(
            skip_reason_for_dump_stderr("mysqldump: Got error: 1142: command denied"),
            Some("insufficient privileges to run the export")
        );
    }

    #[test]
    fn includes_common_dump_command_candidates() {
        let candidates = dump_command_candidates();
        assert!(candidates.contains(&"mysqldump"));
        assert!(candidates.contains(&"/opt/homebrew/opt/mysql-client/bin/mysqldump"));
    }
}
