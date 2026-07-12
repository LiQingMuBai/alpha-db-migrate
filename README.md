# rust-migrate-db

按 `sql` 目录中的文件名，将 SQL 内容导入同名 MySQL 数据库。

例如：

- `sql/gtt.sql` 会导入到 `gtt` 数据库
- `sql/order.sql` 会导入到 `order` 数据库

## 配置方式

所有参数统一放到项目根目录的 `.env`。

示例：

```env
MYSQL_DSN=mysql://your_user:your_password@rm-xxxx.mysql.rds.aliyuncs.com:3306
SQL_DIR=./sql
MYSQL_CHARSET=utf8mb4
DISABLE_FOREIGN_KEY_CHECKS=true
```

其中：

- `MYSQL_DSN`：MySQL 连接串，程序会在运行时自动把数据库名替换成当前 SQL 文件名
- `SQL_DIR`：SQL 文件目录，默认 `./sql`
- `MYSQL_CHARSET`：连接字符集，默认 `utf8mb4`
- `DISABLE_FOREIGN_KEY_CHECKS`：导入前后自动关闭/恢复外键检查，默认 `true`

## 执行方式

```bash
cargo run
```

## 说明

- 只会读取当前目录下扩展名为 `.sql` 的文件
- 会按文件名字母顺序扫描，并为每个 SQL 文件启动一个线程并发导入
- 文件名去掉 `.sql` 后的部分会作为数据库名
- 例如 `.env` 里配置 `MYSQL_DSN=mysql://user:pass@host:3306`，导入 `sql/gtt.sql` 时会连接到 `mysql://user:pass@host:3306/gtt`
- 每个数据库在导入前会先删除当前库内已有的表和视图，再执行对应 SQL 文件
- 如果某个目标数据库不存在，或者当前账号没有权限进入/导入，程序会在终端提醒并跳过，继续处理后续 SQL 文件
- 支持常见 MySQL dump 中的 `DELIMITER` 语法
