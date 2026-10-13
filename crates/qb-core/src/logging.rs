use std::{
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

use tracing::field::{Field, Visit};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{
    field::RecordFields,
    fmt::{
        FormattedFields,
        format::{FormatFields, Writer},
    },
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

const MAX_BYTES: u64 = 50 * 1024 * 1024;
const MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

#[derive(Default, Clone)]
pub struct RedactingFields;

impl<'writer> FormatFields<'writer> for RedactingFields {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'writer>, fields: R) -> fmt::Result {
        let mut visitor = RedactingVisitor {
            writer,
            first: true,
            result: Ok(()),
        };
        fields.record(&mut visitor);
        visitor.result
    }

    fn add_fields(
        &self,
        current: &'writer mut FormattedFields<Self>,
        fields: &tracing::span::Record<'_>,
    ) -> fmt::Result {
        let empty = current.is_empty();
        let mut writer = current.as_writer();
        if !empty {
            write!(writer, " ")?;
        }
        self.format_fields(writer, fields)
    }
}

struct RedactingVisitor<'a> {
    writer: Writer<'a>,
    first: bool,
    result: fmt::Result,
}
impl Visit for RedactingVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if self.result.is_err() {
            return;
        }
        if !self.first {
            self.result = write!(self.writer, " ");
        }
        self.first = false;
        if sensitive(field.name()) {
            self.result = write!(self.writer, "{}=<redacted>", field.name());
        } else {
            self.result = write!(self.writer, "{}={value:?}", field.name());
        }
    }
}

fn sensitive(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace('-', "_");
    // 헤더 묶음과 본문은 내부 값의 필드명을 알 수 없으므로 전체를 가린다.
    name.split('.').any(|part| {
        matches!(
            part,
            "authorization"
                | "x_api_key"
                | "cookie"
                | "access_token"
                | "refresh_token"
                | "id_token"
                | "code"
                | "client_key"
                | "admin_token"
                | "headers"
                | "body"
                | "query"
                | "callback_query"
        )
    })
}

pub fn init(home: &Path) -> anyhow::Result<WorkerGuard> {
    let writer = RollingLog::new(home.join("logs"))?;
    let (writer, guard) = tracing_appender::non_blocking(writer);
    let file = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .fmt_fields(RedactingFields)
        .with_writer(writer);
    let stderr = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .fmt_fields(RedactingFields)
        .with_writer(io::stderr);
    install(file, stderr)?;
    Ok(guard)
}

fn install<F, E>(file: F, stderr: E) -> anyhow::Result<()>
where
    F: tracing_subscriber::Layer<tracing_subscriber::Registry> + Send + Sync + 'static,
    E: tracing_subscriber::Layer<
            tracing_subscriber::layer::Layered<F, tracing_subscriber::Registry>,
        > + Send
        + Sync
        + 'static,
{
    tracing_subscriber::registry()
        .with(file)
        .with(stderr)
        .try_init()?;
    Ok(())
}

struct RollingLog {
    directory: PathBuf,
    file: Option<File>,
    date: String,
    size: u64,
    sequence: u64,
}
impl RollingLog {
    fn new(directory: PathBuf) -> io::Result<Self> {
        fs::create_dir_all(&directory)?;
        let mut writer = Self {
            directory,
            file: None,
            date: String::new(),
            size: 0,
            sequence: 0,
        };
        writer.rotate()?;
        Ok(writer)
    }
    fn rotate(&mut self) -> io::Result<()> {
        self.file.take();
        self.date = chrono::Utc::now().format("%Y-%m-%d").to_string();
        loop {
            let filename = format!(
                "quota-baton.{}.{}.{:04}.log",
                self.date,
                std::process::id(),
                self.sequence
            );
            self.sequence += 1;
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(self.directory.join(filename))
            {
                Ok(file) => {
                    self.file = Some(file);
                    self.size = 0;
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        self.prune(0)
    }
    fn prune(&self, incoming: u64) -> io::Result<()> {
        let mut logs = Vec::new();
        for entry in fs::read_dir(&self.directory)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if !name.starts_with("quota-baton.") || !name.ends_with(".log") {
                continue;
            }
            let metadata = entry.metadata()?;
            if metadata.is_file() {
                logs.push((metadata.modified()?, entry.path(), metadata.len()));
            }
        }
        logs.sort_by_key(|entry| entry.0);
        let mut total: u64 = logs.iter().map(|entry| entry.2).sum();
        let mut count = logs.len();
        for (modified, path, size) in logs {
            let expired = modified.elapsed().unwrap_or_default().as_secs() >= 7 * 24 * 60 * 60;
            if expired || count > 7 || total + incoming > MAX_BYTES {
                // 현재 파일은 가장 최근 파일이며 크기 회전으로 한도 아래에 유지된다.
                if count <= 1 {
                    break;
                }
                fs::remove_file(path)?;
                total -= size;
                count -= 1;
            }
        }
        Ok(())
    }
}
impl Write for RollingLog {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.date != chrono::Utc::now().format("%Y-%m-%d").to_string()
            || self.size + bytes.len() as u64 > MAX_FILE_BYTES
        {
            self.rotate()?;
        }
        // 비정상적으로 큰 단일 로그 레코드는 저장하지 않는다.
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Ok(bytes.len());
        }
        self.prune(bytes.len() as u64)?;
        self.file
            .as_mut()
            .expect("rolling file initialized")
            .write_all(bytes)?;
        self.size += bytes.len() as u64;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        self.file
            .as_mut()
            .expect("rolling file initialized")
            .flush()
    }
}
