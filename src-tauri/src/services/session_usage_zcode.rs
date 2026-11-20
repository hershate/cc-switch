//! Import ZCode's per-request model-io session usage:
//! `<ZCODE_HOME|~/.zcode>/cli/rollout/model-io-sess_<sessionId>.jsonl`.
//!
//! Each line is one complete model request. Usage lives at `/response/usage`
//! (`inputTokens/outputTokens/totalTokens/cacheReadTokens/cacheWriteTokens`);
//! `inputTokens` follows OpenAI cache-inclusive semantics — it already contains
//! `cacheReadTokens` — so rows are stored as-is under `app_type = "zcode"` with
//! `input_token_semantics = TOTAL`, and cost calc plus aggregation normalization
//! go through `CACHE_INCLUSIVE_APP_TYPES`. Interrupted requests commit
//! `"usage": {}`; all-zero rows are skipped. `requestId` and `attempt` (a retry
//! counter, observed values include 11) form the request id, so retried lines
//! import as separate rows. Subagents keep their own files
//! (`model-io-sess_subagent_agent_<uuid>.jsonl`) and need no special handling.
//! Lines can reach ~300KB (full request/response bodies), so files are read
//! incrementally with `read_until`; a trailing partial line waits for the next
//! round. The daily logs under `cli/log/` redact usage to `[Redacted]` and are
//! not usable.

use crate::config::get_home_dir;
use crate::database::{lock_conn, Database};
use crate::error::AppError;
use crate::proxy::usage::parser::TokenUsage;
use crate::services::{
    session_usage::{
        cost_columns, load_sync_cursors, metadata_modified_nanos, update_sync_state_on_conn,
        SessionSyncResult,
    },
    sql_helpers::INPUT_TOKEN_SEMANTICS_TOTAL,
    usage_stats::find_model_pricing,
};
use crate::session_manager::providers::utils::parse_timestamp_to_ms;
use rust_decimal::Decimal;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Seek, SeekFrom},
    path::{Path, PathBuf},
};

const DATA_SOURCE: &str = "zcode_session";
/// Cursor key prefix. Bump when "which records count as a request" changes;
/// the permanent receipts in `session_usage_dedup` keep the forced full re-read
/// from double-counting already-imported requests.
const CURSOR_PREFIX: &str = "zcode:v1:";

pub fn sync_zcode_usage(db: &Database) -> Result<SessionSyncResult, AppError> {
    let home = std::env::var_os("ZCODE_HOME")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| get_home_dir().join(".zcode"));
    sync_rollout(db, &home.join("cli").join("rollout"))
}

fn sync_rollout(db: &Database, root: &Path) -> Result<SessionSyncResult, AppError> {
    let mut result = SessionSyncResult::default();
    let Ok(entries) = fs::read_dir(root) else {
        return Ok(result);
    };
    let mut files: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path.extension().and_then(|ext| ext.to_str()) == Some("jsonl")
                && path
                    .file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("model-io-sess"))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return Ok(result);
    }
    let cursors = load_sync_cursors(db)?;
    for path in files {
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let modified = metadata_modified_nanos(&meta);
        let key = format!("{CURSOR_PREFIX}{}", path.to_string_lossy());
        let cursor = cursors.get(&key);
        // Skip only when the file is untouched AND fully consumed: a trailing
        // partial line (size > bytes read) keeps the file eligible next round.
        if cursor
            .is_some_and(|c| c.last_modified == modified && c.last_line_offset == meta.len() as i64)
        {
            continue;
        }
        let offset = cursor
            .map(|c| c.last_line_offset)
            .filter(|offset| *offset >= 0 && *offset <= meta.len() as i64)
            .unwrap_or(0);
        match sync_file(db, &path, &key, modified, offset) {
            Ok(part) => result.merge(part),
            Err(error) => result
                .errors
                .push(format!("ZCode session {}: {error}", path.display())),
        }
    }
    Ok(result)
}

struct Entry {
    request_id: String,
    session_id: String,
    model: String,
    usage: TokenUsage,
    created_ms: i64,
    latency_ms: i64,
}

/// One committed line → one import candidate. `line` is the raw record bytes,
/// hashed when `requestId` is missing.
fn entry(record: &Value, line: &[u8]) -> Option<Entry> {
    let usage = record.pointer("/response/usage")?;
    let tokens = |field: &str| {
        usage
            .get(field)
            .and_then(Value::as_u64)
            .unwrap_or(0)
            .min(u32::MAX as u64) as u32
    };
    let usage = TokenUsage {
        input_tokens: tokens("inputTokens"),
        output_tokens: tokens("outputTokens"),
        cache_read_tokens: tokens("cacheReadTokens"),
        cache_creation_tokens: tokens("cacheWriteTokens"),
        cache_creation_1h_tokens: 0,
        model: None,
        message_id: None,
    };
    // Interrupted requests commit `"usage": {}` (sample-3 shape); nothing was billed.
    if usage.input_tokens == 0
        && usage.output_tokens == 0
        && usage.cache_read_tokens == 0
        && usage.cache_creation_tokens == 0
    {
        return None;
    }
    let session_id = record
        .get("sessionId")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    let identity = record
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| format!("{:x}", Sha256::digest(line)));
    let attempt = record.get("attempt").and_then(Value::as_i64).unwrap_or(0);
    let model = record
        .pointer("/model/modelId")
        .and_then(Value::as_str)
        .or_else(|| {
            record
                .pointer("/request/body/model")
                .and_then(Value::as_str)
        })
        .unwrap_or("unknown")
        .to_string();
    let created_ms = record
        .get("startedAt")
        .and_then(parse_timestamp_to_ms)
        .or_else(|| record.get("completedAt").and_then(parse_timestamp_to_ms))?;
    let latency_ms = record
        .get("durationMs")
        .and_then(Value::as_i64)
        .unwrap_or(0)
        .clamp(0, 3_600_000);
    Some(Entry {
        request_id: format!("zcode:{session_id}:{identity}:{attempt}"),
        session_id,
        usage: TokenUsage {
            model: Some(model.clone()),
            ..usage
        },
        model,
        created_ms,
        latency_ms,
    })
}

fn sync_file(
    db: &Database,
    path: &Path,
    key: &str,
    modified: i64,
    offset: i64,
) -> Result<SessionSyncResult, AppError> {
    let mut file = fs::File::open(path).map_err(|error| AppError::io(path, error))?;
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|error| AppError::io(path, error))?;
    let mut reader = BufReader::new(file);
    let mut buffer = Vec::new();
    let mut bytes = offset;
    let mut entries = Vec::new();
    loop {
        buffer.clear();
        let size = reader
            .read_until(b'\n', &mut buffer)
            .map_err(|error| AppError::io(path, error))?;
        // The writer may have flushed only part of the last record; retry next round.
        if size == 0 || buffer.last() != Some(&b'\n') {
            break;
        }
        bytes += size as i64;
        if let Some(entry) = serde_json::from_slice::<Value>(&buffer)
            .ok()
            .as_ref()
            .and_then(|record| entry(record, &buffer))
        {
            entries.push(entry);
        }
    }

    let mut result = SessionSyncResult {
        files_scanned: 1,
        ..Default::default()
    };
    let mut conn = lock_conn!(db.conn);
    let tx = conn.transaction()?;
    for entry in entries {
        if tx.execute(
            "INSERT OR IGNORE INTO session_usage_dedup (data_source, request_id, semantic_id, has_entry_id) VALUES (?1, ?2, ?2, 1)",
            [DATA_SOURCE, &entry.request_id],
        )? == 0
        {
            result.skipped += 1;
            continue;
        }
        let usage = &entry.usage;
        let amounts = cost_columns(
            "zcode",
            usage,
            find_model_pricing(&tx, &entry.model).as_ref(),
            Decimal::ONE,
        );
        tx.execute(
            "INSERT OR IGNORE INTO proxy_request_logs (
                request_id, provider_id, app_type, model, request_model, input_tokens, output_tokens, cache_read_tokens, cache_creation_tokens,
                input_cost_usd, output_cost_usd, cache_read_cost_usd, cache_creation_cost_usd, total_cost_usd,
                latency_ms, status_code, session_id, provider_type, is_streaming, cost_multiplier, created_at, data_source, input_token_semantics
            ) VALUES (?1, '_zcode_session', 'zcode', ?2, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 200, ?13, ?14, 1, '1', ?15, ?14, ?16)",
            rusqlite::params![
                entry.request_id,
                entry.model,
                usage.input_tokens,
                usage.output_tokens,
                usage.cache_read_tokens,
                usage.cache_creation_tokens,
                amounts[0],
                amounts[1],
                amounts[2],
                amounts[3],
                amounts[4],
                entry.latency_ms,
                entry.session_id,
                DATA_SOURCE,
                entry.created_ms / 1000,
                INPUT_TOKEN_SEMANTICS_TOTAL,
            ],
        )?;
        result.imported += 1;
    }
    update_sync_state_on_conn(&tx, key, modified, bytes)?;
    tx.commit()?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Fixture mirroring TEMP/zcode-samples/sample-1.jsonl (trimmed).
    fn model_io(request_id: &str, attempt: i64, usage: Value) -> Value {
        serde_json::json!({
            "type": "model_io",
            "sessionId": "sess_335a06d7-c664-47b7-8d3d-073596fc7a9d",
            "turnId": "turn_3b0a2945-b2df-4688-81ef-445d9a411657",
            "querySource": "main_turn",
            "requestId": request_id,
            "attempt": attempt,
            "traceId": "483fbbb7-4864-4f69-b2a7-cb20454b229f",
            "startedAt": "2026-11-20T14:59:15.079Z",
            "completedAt": "2026-11-20T15:00:31.388Z",
            "durationMs": 76309,
            "model": {"modelId": "GLM-5.3", "providerId": "account:bigmodel-individual-coding-plan"},
            "request": {"body": {"model": "GLM-5.3", "max_tokens": 128000, "stream": true}},
            "response": {"finishReason": "tool-calls", "responseId": "msg_1", "usage": usage},
        })
    }

    fn usage(input: u64, output: u64, cache_read: u64, cache_write: u64) -> Value {
        serde_json::json!({
            "inputTokens": input,
            "outputTokens": output,
            "totalTokens": input + output,
            "cacheReadTokens": cache_read,
            "cacheWriteTokens": cache_write,
        })
    }

    fn session_file(root: &Path) -> PathBuf {
        root.join("model-io-sess_335a06d7-c664-47b7-8d3d-073596fc7a9d.jsonl")
    }

    fn write_lines(path: &Path, records: &[Value]) {
        let mut file = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        for record in records {
            writeln!(file, "{record}").unwrap();
        }
    }

    /// (request_id, model, session_id, input, output, cache_read, cache_write,
    ///  latency_ms, created_at, provider_id, data_source, input_token_semantics)
    type UsageRow = (
        String,
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
        String,
        String,
        i64,
    );

    fn rows(db: &Database) -> Vec<UsageRow> {
        let conn = db.conn.lock().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT request_id, model, session_id, input_tokens, output_tokens,
                        cache_read_tokens, cache_creation_tokens, latency_ms, created_at,
                        provider_id, data_source, input_token_semantics
                 FROM proxy_request_logs WHERE app_type = 'zcode' ORDER BY request_id",
            )
            .unwrap();
        stmt.query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
            ))
        })
        .unwrap()
        .map(Result::unwrap)
        .collect()
    }

    fn epoch_seconds(iso: &str) -> i64 {
        chrono::DateTime::parse_from_rfc3339(iso)
            .unwrap()
            .timestamp()
    }

    #[test]
    fn zcode_usage_imports_model_io_records() {
        let dir = tempfile::tempdir().unwrap();
        // A stray non-model-io file in the same directory must be ignored.
        fs::write(
            dir.path().join("other.jsonl"),
            format!("{}\n", model_io("stray", 1, usage(1, 1, 0, 0))),
        )
        .unwrap();
        write_lines(
            &session_file(dir.path()),
            &[model_io("req-1", 1, usage(121664, 4142, 16832, 0))],
        );
        let db = Database::memory().unwrap();
        let result = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!(
            (result.imported, result.skipped, result.files_scanned),
            (1, 0, 1)
        );
        assert_eq!(
            rows(&db),
            vec![(
                "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:1".into(),
                "GLM-5.3".into(),
                "sess_335a06d7-c664-47b7-8d3d-073596fc7a9d".into(),
                121664,
                4142,
                16832,
                0,
                76309,
                epoch_seconds("2026-11-20T14:59:15.079Z"),
                "_zcode_session".into(),
                "zcode_session".into(),
                INPUT_TOKEN_SEMANTICS_TOTAL,
            )]
        );
    }

    #[test]
    fn zcode_usage_imports_appended_records_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path());
        write_lines(&path, &[model_io("req-1", 1, usage(1000, 10, 100, 0))]);
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        // Unchanged file: the cursor gate skips it entirely.
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 0);

        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{}", model_io("req-2", 1, usage(2000, 20, 200, 0))).unwrap();
        write!(file, "{{\"type\":\"model_io\",\"req").unwrap();
        drop(file);
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        assert_eq!(rows(&db).len(), 2);
    }

    #[test]
    fn zcode_usage_retries_partial_tail_next_round() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path());
        write_lines(&path, &[model_io("req-1", 1, usage(1000, 10, 100, 0))]);
        // The writer flushes a prefix of the next record (no trailing newline).
        let full = model_io("req-2", 1, usage(3000, 30, 300, 50)).to_string();
        let (head, tail) = full.split_at(12);
        assert_eq!(head, "{\"type\":\"mod");
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        write!(file, "{head}").unwrap();
        drop(file);
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        assert_eq!(rows(&db).len(), 1);

        // The record completes: the cursor never passed it, so it imports now.
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        writeln!(file, "{tail}").unwrap();
        drop(file);
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 1);
        let imported = rows(&db);
        assert_eq!(imported.len(), 2);
        assert_eq!(imported[1].3, 3000);
    }

    #[test]
    fn zcode_usage_receipts_prevent_double_import_on_rescan() {
        let dir = tempfile::tempdir().unwrap();
        let path = session_file(dir.path());
        let record = model_io("req-1", 1, usage(1000, 10, 100, 0));
        write_lines(&path, &[record.clone(), record.clone()]);
        let db = Database::memory().unwrap();
        let first = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!((first.imported, first.skipped), (1, 1));

        // The file shrinks below the cursor (rewrite): read restarts from 0,
        // the receipt blocks the already-imported record.
        fs::write(&path, format!("{record}\n")).unwrap();
        let file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(file);
        let result = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!((result.imported, result.skipped), (0, 1));
        assert_eq!(rows(&db).len(), 1);
    }

    #[test]
    fn zcode_usage_separates_retry_attempts() {
        let dir = tempfile::tempdir().unwrap();
        let mut retried = model_io("req-1", 11, usage(2000, 20, 200, 0));
        // Oversized duration clamps to one hour.
        retried["durationMs"] = serde_json::json!(9_000_000_000_i64);
        write_lines(
            &session_file(dir.path()),
            &[model_io("req-1", 1, usage(1000, 10, 100, 0)), retried],
        );
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 2);
        let imported = rows(&db);
        assert_eq!(
            imported[0].0,
            "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:1"
        );
        assert_eq!((imported[0].3, imported[0].7), (1000, 76309));
        assert_eq!(
            imported[1].0,
            "zcode:sess_335a06d7-c664-47b7-8d3d-073596fc7a9d:req-1:11"
        );
        assert_eq!((imported[1].3, imported[1].7), (2000, 3_600_000));
    }

    #[test]
    fn zcode_usage_skips_records_without_usage() {
        let dir = tempfile::tempdir().unwrap();
        // sample-3 shape: response without a usable usage object.
        let empty_usage = model_io("req-1", 1, serde_json::json!({}));
        let mut no_usage = model_io("req-2", 1, serde_json::json!({}));
        no_usage["response"]
            .as_object_mut()
            .unwrap()
            .remove("usage");
        let all_zero = model_io("req-3", 1, usage(0, 0, 0, 0));
        write_lines(
            &session_file(dir.path()),
            &[empty_usage, no_usage, all_zero],
        );
        let db = Database::memory().unwrap();
        let result = sync_rollout(&db, dir.path()).unwrap();
        assert_eq!((result.imported, result.skipped), (0, 0));
        assert!(rows(&db).is_empty());
    }

    #[test]
    fn zcode_usage_parses_iso8601_timestamps_and_model_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let mut fallback_model = model_io("req-1", 1, usage(1000, 10, 100, 0));
        fallback_model["model"]
            .as_object_mut()
            .unwrap()
            .remove("modelId");
        fallback_model["request"]["body"]["model"] = serde_json::json!("MiniMax-M3.1-Flash");
        let mut no_started = model_io("req-2", 1, usage(2000, 20, 200, 0));
        no_started.as_object_mut().unwrap().remove("startedAt");
        no_started["completedAt"] = serde_json::json!("2026-11-19T10:00:00Z");
        write_lines(&session_file(dir.path()), &[fallback_model, no_started]);
        let db = Database::memory().unwrap();
        assert_eq!(sync_rollout(&db, dir.path()).unwrap().imported, 2);
        let imported = rows(&db);
        // Model falls back to /request/body/model.
        assert_eq!(imported[0].1, "MiniMax-M3.1-Flash");
        assert_eq!(imported[0].8, epoch_seconds("2026-11-20T14:59:15.079Z"));
        // startedAt missing: completedAt (second precision, no fraction) is used.
        assert_eq!(imported[1].8, epoch_seconds("2026-11-19T10:00:00Z"));
    }

    #[test]
    fn zcode_usage_skips_missing_rollout_dir() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::memory().unwrap();
        let result = sync_rollout(&db, &dir.path().join("missing")).unwrap();
        assert_eq!(result.imported, 0);
        assert!(result.errors.is_empty());
    }
}
