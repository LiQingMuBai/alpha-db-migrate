# rust-migrate-db

Import SQL files into MySQL databases by matching each file name to a database name.

Examples:

- `sql/gtt.sql` will be imported into the `gtt` database
- `sql/order.sql` will be imported into the `order` database

## Configuration

Put all configuration values in the project root `.env` file.
No runtime CLI parameters are required for either import or export.

Example:

```env
MYSQL_DSN=mysql://your_user:your_password@rm-xxxx.mysql.rds.aliyuncs.com:3306
SQL_DIR=./sql
MYSQL_CHARSET=utf8mb4
DISABLE_FOREIGN_KEY_CHECKS=true
IGNORE_DATABASES=gtt,order
```

Variables:

- `MYSQL_DSN`: Base MySQL connection string. The program replaces the database name at runtime with the current SQL file name.
- `SQL_DIR`: Directory containing SQL files. Default: `./sql`
- `MYSQL_CHARSET`: Connection charset. Default: `utf8mb4`
- `DISABLE_FOREIGN_KEY_CHECKS`: Disables and restores foreign key checks around the import. Default: `true`
- `IGNORE_DATABASES`: Comma-separated database names to skip entirely. Skipped databases will not be reset or imported.

## Run

```bash
cargo run
```

## Export Configuration

The export tool uses the same `.env` file and adds these variables:

- `EXPORT_DATABASES`: Comma-separated database names to export. Required for export.
- `EXPORT_SQL_DIR`: Directory for generated dump files. Default: `./export_sql`

Example:

```env
MYSQL_DSN=mysql://your_user:your_password@rm-xxxx.mysql.rds.aliyuncs.com:3306
EXPORT_DATABASES=gtt,order
EXPORT_SQL_DIR=./export_sql
IGNORE_DATABASES=order
```

## Export Run

```bash
cargo run --bin export
```

## Notes

- Only `.sql` files in the configured directory are scanned
- Files are scanned in alphabetical order, and each SQL file runs in its own worker thread
- The file name without the `.sql` suffix is used as the target database name
- Example: if `.env` contains `MYSQL_DSN=mysql://user:pass@host:3306`, importing `sql/gtt.sql` connects to `mysql://user:pass@host:3306/gtt`
- Before importing, the tool drops existing tables and views in the target database
- Databases listed in `IGNORE_DATABASES` are skipped entirely before any reset or import work starts
- If a target database does not exist, or the current account does not have access to enter or import it, the tool prints a terminal notice and skips that file
- Common MySQL dump files with `DELIMITER` statements are supported
- The export tool writes one dump file per database, for example `export_sql/gtt.sql`
- The export tool requires `mysqldump` to be installed and available in `PATH`
