# rust-migrate-db

Import SQL files into MySQL databases by matching each file name to a database name.

Examples:

- `sql/gtt.sql` will be imported into the `gtt` database
- `sql/order.sql` will be imported into the `order` database

## Configuration

Put all configuration values in the project root `.env` file.

Example:

```env
MYSQL_DSN=mysql://your_user:your_password@rm-xxxx.mysql.rds.aliyuncs.com:3306
SQL_DIR=./sql
MYSQL_CHARSET=utf8mb4
DISABLE_FOREIGN_KEY_CHECKS=true
```

Variables:

- `MYSQL_DSN`: Base MySQL connection string. The program replaces the database name at runtime with the current SQL file name.
- `SQL_DIR`: Directory containing SQL files. Default: `./sql`
- `MYSQL_CHARSET`: Connection charset. Default: `utf8mb4`
- `DISABLE_FOREIGN_KEY_CHECKS`: Disables and restores foreign key checks around the import. Default: `true`

## Run

```bash
cargo run
```

## Notes

- Only `.sql` files in the configured directory are scanned
- Files are scanned in alphabetical order, and each SQL file runs in its own worker thread
- The file name without the `.sql` suffix is used as the target database name
- Example: if `.env` contains `MYSQL_DSN=mysql://user:pass@host:3306`, importing `sql/gtt.sql` connects to `mysql://user:pass@host:3306/gtt`
- Before importing, the tool drops existing tables and views in the target database
- If a target database does not exist, or the current account does not have access to enter or import it, the tool prints a terminal notice and skips that file
- Common MySQL dump files with `DELIMITER` statements are supported
