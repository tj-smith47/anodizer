//! Streamed uploads, and the store double they are observed through.

use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use async_trait::async_trait;
use futures_core::stream::BoxStream;
use object_store::local::LocalFileSystem;
use object_store::memory::InMemory;
use object_store::path::Path as ObjectPath;
use object_store::{
    Attribute, Attributes, CopyOptions, GetOptions, GetRange, GetResult, ListResult,
    MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt, PutMultipartOptions, PutOptions,
    PutPayload, PutResult, UploadPart,
};
use tokio::io::{AsyncRead, ReadBuf};

use anodizer_core::config::BlobConfig;
use anodizer_core::log::{StageLogger, Verbosity};

use crate::provider::Provider;
use crate::tests::make_ctx;
use crate::upload::{
    ABORT_TIMEOUT, UPLOAD_PART_BYTES, UPLOAD_PART_CONCURRENCY, UploadDestination, UploadReport,
    UploadSink, build_put_options, incomplete_upload_warning, object_matches_reader, stream_upload,
    upload_files_owned,
};

// -----------------------------------------------------------------------
// The store double
// -----------------------------------------------------------------------

/// The store error a [`Probe`] raises in place of the real call.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Fault {
    NotFound,
    PermissionDenied,
    Generic,
}

impl Fault {
    fn error(self, path: &str) -> object_store::Error {
        let source = "injected failure".into();
        let path = path.to_string();
        match self {
            Fault::NotFound => object_store::Error::NotFound { path, source },
            Fault::PermissionDenied => object_store::Error::PermissionDenied { path, source },
            Fault::Generic => object_store::Error::Generic {
                store: "Probe",
                source,
            },
        }
    }
}

/// Which calls of a [`Probe`] fail, and how. Indexes count from zero.
#[derive(Clone, Debug, Default)]
pub(crate) struct Faults {
    /// A single PUT to a key ending with the suffix fails.
    pub put: Option<(Fault, &'static str)>,
    /// Opening a multipart upload fails.
    pub create: Option<Fault>,
    /// The part with this index fails.
    pub part: Option<(usize, Fault)>,
    pub complete: Option<Fault>,
    pub abort: Option<Fault>,
    pub abort_hangs: bool,
    /// The ranged read with this index fails.
    pub range: Option<usize>,
    /// The object is replaced with these bytes just before the ranged read
    /// with this index.
    pub swap_before_range: Option<(usize, Vec<u8>)>,
    /// How long each part takes, so parts overlap.
    pub part_delay: Duration,
    /// Open multipart uploads without their attributes, for an inner store
    /// that refuses them (`LocalFileSystem`).
    pub drop_multipart_attributes: bool,
}

/// What a [`Probe`] was asked to do.
#[derive(Debug, Default)]
pub(crate) struct Seen {
    pub put_sizes: Mutex<Vec<usize>>,
    pub put_attributes: Mutex<Vec<Attributes>>,
    pub part_sizes: Mutex<Vec<usize>>,
    pub multipart_attributes: Mutex<Vec<Attributes>>,
    pub range_sizes: Mutex<Vec<u64>>,
    pub range_if_match: Mutex<Vec<Option<String>>>,
    in_flight: AtomicUsize,
    pub max_in_flight: AtomicUsize,
    pub aborts: AtomicUsize,
}

/// A store that records the size of every payload, part and range it is
/// handed, and fails the calls its [`Faults`] name.
#[derive(Debug)]
pub(crate) struct Probe {
    inner: Arc<dyn ObjectStore>,
    faults: Faults,
    seen: Arc<Seen>,
}

impl Probe {
    pub(crate) fn over(
        inner: Arc<dyn ObjectStore>,
        faults: Faults,
    ) -> (Arc<dyn ObjectStore>, Arc<Seen>) {
        let seen = Arc::new(Seen::default());
        let probe = Probe {
            inner,
            faults,
            seen: Arc::clone(&seen),
        };
        (Arc::new(probe), seen)
    }

    pub(crate) fn in_memory(faults: Faults) -> (Arc<dyn ObjectStore>, Arc<Seen>) {
        Self::over(Arc::new(InMemory::new()), faults)
    }
}

impl std::fmt::Display for Probe {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Probe({})", self.inner)
    }
}

#[async_trait]
impl ObjectStore for Probe {
    async fn put_opts(
        &self,
        location: &ObjectPath,
        payload: PutPayload,
        opts: PutOptions,
    ) -> object_store::Result<PutResult> {
        self.seen
            .put_sizes
            .lock()
            .unwrap()
            .push(payload.content_length());
        self.seen
            .put_attributes
            .lock()
            .unwrap()
            .push(opts.attributes.clone());
        if let Some((fault, suffix)) = self.faults.put
            && location.as_ref().ends_with(suffix)
        {
            return Err(fault.error(location.as_ref()));
        }
        self.inner.put_opts(location, payload, opts).await
    }

    async fn put_multipart_opts(
        &self,
        location: &ObjectPath,
        opts: PutMultipartOptions,
    ) -> object_store::Result<Box<dyn MultipartUpload>> {
        self.seen
            .multipart_attributes
            .lock()
            .unwrap()
            .push(opts.attributes.clone());
        if let Some(fault) = self.faults.create {
            return Err(fault.error(location.as_ref()));
        }
        let opts = if self.faults.drop_multipart_attributes {
            PutMultipartOptions::default()
        } else {
            opts
        };
        let inner = self.inner.put_multipart_opts(location, opts).await?;
        Ok(Box::new(ProbeUpload {
            inner,
            faults: self.faults.clone(),
            seen: Arc::clone(&self.seen),
            location: location.to_string(),
            parts: 0,
        }))
    }

    async fn get_opts(
        &self,
        location: &ObjectPath,
        options: GetOptions,
    ) -> object_store::Result<GetResult> {
        if let Some(range) = &options.range {
            let index = {
                let mut sizes = self.seen.range_sizes.lock().unwrap();
                sizes.push(match range {
                    GetRange::Bounded(r) => r.end - r.start,
                    _ => u64::MAX,
                });
                sizes.len() - 1
            };
            self.seen
                .range_if_match
                .lock()
                .unwrap()
                .push(options.if_match.clone());
            if self.faults.range == Some(index) {
                return Err(Fault::Generic.error(location.as_ref()));
            }
            if let Some((at, bytes)) = &self.faults.swap_before_range
                && *at == index
            {
                self.inner.put(location, bytes.clone().into()).await?;
            }
        }
        self.inner.get_opts(location, options).await
    }

    fn delete_stream(
        &self,
        locations: BoxStream<'static, object_store::Result<ObjectPath>>,
    ) -> BoxStream<'static, object_store::Result<ObjectPath>> {
        self.inner.delete_stream(locations)
    }

    fn list(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> BoxStream<'static, object_store::Result<ObjectMeta>> {
        self.inner.list(prefix)
    }

    async fn list_with_delimiter(
        &self,
        prefix: Option<&ObjectPath>,
    ) -> object_store::Result<ListResult> {
        self.inner.list_with_delimiter(prefix).await
    }

    async fn copy_opts(
        &self,
        from: &ObjectPath,
        to: &ObjectPath,
        options: CopyOptions,
    ) -> object_store::Result<()> {
        self.inner.copy_opts(from, to, options).await
    }
}

#[derive(Debug)]
struct ProbeUpload {
    inner: Box<dyn MultipartUpload>,
    faults: Faults,
    seen: Arc<Seen>,
    location: String,
    parts: usize,
}

#[async_trait]
impl MultipartUpload for ProbeUpload {
    fn put_part(&mut self, data: PutPayload) -> UploadPart {
        let index = self.parts;
        self.parts += 1;
        self.seen
            .part_sizes
            .lock()
            .unwrap()
            .push(data.content_length());
        let fault = self
            .faults
            .part
            .filter(|(at, _)| *at == index)
            .map(|(_, fault)| fault);
        let upload = self.inner.put_part(data);
        let seen = Arc::clone(&self.seen);
        let delay = self.faults.part_delay;
        let location = self.location.clone();
        Box::pin(async move {
            let now = seen.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            seen.max_in_flight.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(delay).await;
            let result = match fault {
                Some(fault) => Err(fault.error(&location)),
                None => upload.await,
            };
            seen.in_flight.fetch_sub(1, Ordering::SeqCst);
            result
        })
    }

    async fn complete(&mut self) -> object_store::Result<PutResult> {
        if let Some(fault) = self.faults.complete {
            return Err(fault.error(&self.location));
        }
        self.inner.complete().await
    }

    async fn abort(&mut self) -> object_store::Result<()> {
        self.seen.aborts.fetch_add(1, Ordering::SeqCst);
        if self.faults.abort_hangs {
            std::future::pending::<()>().await;
        }
        if let Some(fault) = self.faults.abort {
            return Err(fault.error(&self.location));
        }
        self.inner.abort().await
    }
}

// -----------------------------------------------------------------------
// Helpers
// -----------------------------------------------------------------------

const PART: usize = 64 * 1024;

fn pattern(total: usize) -> Vec<u8> {
    let block: Vec<u8> = (0..251u8).collect();
    let mut out = Vec::with_capacity(total + block.len());
    while out.len() < total {
        out.extend_from_slice(&block);
    }
    out.truncate(total);
    out
}

/// Hands out `pattern(total)` a few kilobytes per read without ever
/// holding it, calls `before_read` with the bytes handed out so far, and
/// fails once `fail_at` bytes are out.
struct PatternReader<F: FnMut(usize) + Unpin> {
    total: usize,
    emitted: usize,
    fail_at: Option<usize>,
    before_read: F,
}

fn reader(total: usize, fail_at: Option<usize>) -> PatternReader<fn(usize)> {
    PatternReader {
        total,
        emitted: 0,
        fail_at,
        before_read: |_| {},
    }
}

impl<F: FnMut(usize) + Unpin> AsyncRead for PatternReader<F> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let emitted = self.emitted;
        (self.before_read)(emitted);
        if self.fail_at.is_some_and(|at| emitted >= at) {
            return Poll::Ready(Err(std::io::Error::other("simulated read failure")));
        }
        let limit = self.fail_at.unwrap_or(self.total).min(self.total);
        let n = (limit - emitted).min(buf.remaining()).min(8 * 1024);
        for i in emitted..emitted + n {
            buf.put_slice(&[(i % 251) as u8]);
        }
        self.emitted += n;
        Poll::Ready(Ok(()))
    }
}

fn files_under(dir: &std::path::Path) -> Vec<(String, u64)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        if meta.is_dir() {
            out.extend(files_under(&entry.path()));
        } else {
            out.push((entry.file_name().to_string_lossy().into_owned(), meta.len()));
        }
    }
    out
}

/// A store on the local disk, where a staged multipart upload is a file
/// the test can see.
fn on_disk(root: &std::path::Path) -> Arc<dyn ObjectStore> {
    Probe::over(
        Arc::new(LocalFileSystem::new_with_prefix(root).unwrap()),
        Faults {
            drop_multipart_attributes: true,
            ..Default::default()
        },
    )
    .0
}

fn quiet() -> StageLogger {
    StageLogger::new("blob-test", Verbosity::Quiet)
}

/// A sink with parts small enough that a test file of a megabyte is many
/// parts long, one part in flight.
fn small_sink(store: &Arc<dyn ObjectStore>, key: &str) -> UploadSink {
    UploadSink {
        part_bytes: PART,
        concurrency: 1,
        ..UploadSink::new(
            Arc::clone(store),
            UploadDestination::test_s3(),
            ObjectPath::from(key),
            Attributes::new(),
        )
    }
}

async fn stored(store: &Arc<dyn ObjectStore>, key: &str) -> (Vec<u8>, Attributes) {
    let got = store.get(&ObjectPath::from(key)).await.unwrap();
    let attributes = got.attributes.clone();
    (got.bytes().await.unwrap().to_vec(), attributes)
}

fn content_type(attributes: &Attributes) -> Option<&str> {
    attributes.get(&Attribute::ContentType).map(|v| v.as_ref())
}

/// Run `upload_files_owned` over `files` (local path, object name) under
/// `dist/`, with the headers `config` asks for.
fn publish(
    rt: &tokio::runtime::Runtime,
    store: &Arc<dyn ObjectStore>,
    files: &[(std::path::PathBuf, &str)],
    config: &BlobConfig,
    log: &StageLogger,
) -> (UploadReport, anyhow::Result<()>) {
    let ctx = make_ctx();
    let items: Vec<(std::path::PathBuf, String)> = files
        .iter()
        .map(|(path, name)| (path.clone(), name.to_string()))
        .collect();
    let opts: Vec<PutOptions> = items
        .iter()
        .map(|(_, name)| build_put_options(config, name, &ctx).unwrap())
        .collect();
    upload_files_owned(
        rt,
        Arc::clone(store),
        items,
        "dist".to_string(),
        opts,
        2,
        None,
        UploadDestination::test_s3(),
        log,
    )
}

// -----------------------------------------------------------------------
// stream_upload
// -----------------------------------------------------------------------

#[tokio::test]
async fn parts_reach_the_store_before_the_file_has_been_read_to_its_end() {
    let root = tempfile::TempDir::new().unwrap();
    let store = on_disk(root.path());
    let total = 16 * PART + 4321;

    // With one part in flight at a time, a part is on disk before the
    // part after next is accepted, so half way through the read the
    // store's staging file already holds bytes.
    let staged_mid_read = Arc::new(AtomicBool::new(false));
    let seen = Arc::clone(&staged_mid_read);
    let dir = root.path().to_path_buf();
    let reader = PatternReader {
        total,
        emitted: 0,
        fail_at: None,
        before_read: move |emitted| {
            if emitted >= 8 * PART
                && emitted < total
                && files_under(&dir).iter().any(|(_, len)| *len > 0)
            {
                seen.store(true, Ordering::SeqCst);
            }
        },
    };

    stream_upload(
        reader,
        small_sink(&store, "dist/big.bin"),
        "big.bin",
        "dist/big.bin",
        &quiet(),
    )
    .await
    .unwrap();

    assert!(
        staged_mid_read.load(Ordering::SeqCst),
        "no part had reached the store while the reader still had bytes left"
    );
    assert_eq!(stored(&store, "dist/big.bin").await.0, pattern(total));
    assert_eq!(
        files_under(root.path()),
        vec![("big.bin".to_string(), total as u64)]
    );
}

#[tokio::test]
async fn a_read_that_fails_part_way_commits_nothing_and_a_second_attempt_uploads_it_all() {
    // Below one part the writer is still buffering; above it a multipart
    // upload is already open.
    for fail_at in [100, 5 * PART + 100] {
        let root = tempfile::TempDir::new().unwrap();
        let store = on_disk(root.path());
        let total = 9 * PART + 17;
        let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);

        let err = stream_upload(
            reader(total, Some(fail_at)),
            small_sink(&store, "dist/app.bin"),
            "app.bin",
            "dist/app.bin",
            &log,
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "blobs: read file for upload: app.bin: simulated read failure"
        );
        assert!(
            matches!(
                store.head(&ObjectPath::from("dist/app.bin")).await,
                Err(object_store::Error::NotFound { .. })
            ),
            "a failed read left an object behind (fail_at={fail_at})"
        );
        assert_eq!(
            files_under(root.path()),
            Vec::<(String, u64)>::new(),
            "a failed read left a staged upload behind (fail_at={fail_at})"
        );
        assert_eq!(
            captured.warn_messages(),
            Vec::<String>::new(),
            "an abort that went through warned anyway (fail_at={fail_at})"
        );

        stream_upload(
            reader(total, None),
            small_sink(&store, "dist/app.bin"),
            "app.bin",
            "dist/app.bin",
            &log,
        )
        .await
        .unwrap();
        assert_eq!(stored(&store, "dist/app.bin").await.0, pattern(total));
    }
}

#[tokio::test]
async fn content_of_every_size_is_uploaded_byte_for_byte() {
    let store: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    // Around the sniffed prefix and around one part.
    for total in [0, 10, 511, 512, 513, PART - 1, PART, PART + 1, 3 * PART + 5] {
        let key = format!("dist/{total}.bin");
        stream_upload(
            reader(total, None),
            small_sink(&store, &key),
            "f",
            &key,
            &quiet(),
        )
        .await
        .unwrap();
        assert_eq!(stored(&store, &key).await.0, pattern(total), "{total}");
    }
}

#[tokio::test]
async fn a_store_failure_is_reported_as_an_upload_error_for_the_key() {
    // A regular file sits where the object's parent directory has to go.
    let root = tempfile::TempDir::new().unwrap();
    std::fs::write(root.path().join("dist"), b"not a directory").unwrap();
    let store = on_disk(root.path());
    let err = stream_upload(
        reader(3 * PART, None),
        small_sink(&store, "dist/app.bin"),
        "app.bin",
        "dist/app.bin",
        &quiet(),
    )
    .await
    .unwrap_err();
    assert!(
        err.to_string()
            .starts_with("blobs: upload failed for app.bin → dist/app.bin: "),
        "{err}"
    );
}

/// The type is decided from the first bytes and set on a single PUT and on
/// a multipart upload alike, and the bytes read to decide it are uploaded.
#[tokio::test]
async fn the_content_type_is_set_from_the_first_bytes_on_both_paths() {
    let gzip = |total: usize| {
        let mut data = pattern(total);
        data[..3].copy_from_slice(b"\x1f\x8b\x08");
        data
    };
    let text = |total: usize| vec![b'a'; total];
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (Vec::new(), "text/plain; charset=utf-8"),
        (
            b"{\"version\":\"1.0.0\"}\n".to_vec(),
            "text/plain; charset=utf-8",
        ),
        (text(4 * PART), "text/plain; charset=utf-8"),
        (gzip(100), "application/x-gzip"),
        (gzip(4 * PART + 9), "application/x-gzip"),
        (
            b"\x7fELF\x02\x01\x01\x00".to_vec(),
            "application/octet-stream",
        ),
        (pattern(4 * PART), "application/octet-stream"),
    ];
    for (index, (data, expected)) in cases.into_iter().enumerate() {
        let (store, seen) = Probe::in_memory(Faults::default());
        let key = format!("dist/{index}");
        let mut sink = small_sink(&store, &key);
        sink.attributes
            .insert(Attribute::CacheControl, "max-age=60".into());
        stream_upload(
            std::io::Cursor::new(data.clone()),
            sink,
            "f",
            &key,
            &quiet(),
        )
        .await
        .unwrap();

        let (bytes, attributes) = stored(&store, &key).await;
        assert!(bytes == data, "case {index}: stored bytes differ");
        assert_eq!(content_type(&attributes), Some(expected), "case {index}");
        assert_eq!(
            attributes.get(&Attribute::CacheControl).map(|v| v.as_ref()),
            Some("max-age=60"),
            "case {index}"
        );
        let multipart = data.len() >= PART;
        assert_eq!(
            seen.multipart_attributes.lock().unwrap().len(),
            usize::from(multipart),
            "case {index}"
        );
        assert_eq!(
            seen.put_sizes.lock().unwrap().len(),
            usize::from(!multipart),
            "case {index}"
        );
    }
}

// -----------------------------------------------------------------------
// A failed upload's abort
// -----------------------------------------------------------------------

/// Upload a file whose read fails five parts in, against `faults`, and
/// return the error, the warnings and what the store saw.
async fn read_failure_mid_upload(faults: Faults) -> (String, Vec<String>, Arc<Seen>) {
    let (store, seen) = Probe::in_memory(faults);
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    let sink = UploadSink {
        abort_timeout: Duration::from_millis(20),
        ..small_sink(&store, "dist/app.bin")
    };
    let err = stream_upload(
        reader(9 * PART, Some(5 * PART + 100)),
        sink,
        "app.bin",
        "dist/app.bin",
        &log,
    )
    .await
    .unwrap_err();
    (err.to_string(), captured.warn_messages(), seen)
}

#[tokio::test]
async fn an_abort_that_fails_warns_that_parts_may_remain_at_the_key() {
    let (err, warnings, seen) = read_failure_mid_upload(Faults {
        abort: Some(Fault::Generic),
        ..Default::default()
    })
    .await;
    assert_eq!(
        err,
        "blobs: read file for upload: app.bin: simulated read failure"
    );
    assert_eq!(seen.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(
        warnings,
        vec![
            "an incomplete multipart upload may remain at s3://bucket/dist/app.bin (aborting \
             it failed: Generic Probe error: injected failure); its parts are stored and \
             billed until it is aborted — find its upload ID with `aws s3api \
             list-multipart-uploads --bucket 'bucket' --prefix 'dist/app.bin'` and remove it \
             with `aws s3api abort-multipart-upload --bucket 'bucket' --key 'dist/app.bin' \
             --upload-id <UploadId>`, or set an AbortIncompleteMultipartUpload lifecycle \
             rule on the bucket"
                .to_string()
        ]
    );
}

/// A store that answers the abort with "no such upload" holds no parts, so
/// there is nothing to warn about.
#[tokio::test]
async fn an_abort_of_an_upload_the_store_no_longer_has_warns_nothing() {
    let (_, warnings, seen) = read_failure_mid_upload(Faults {
        abort: Some(Fault::NotFound),
        ..Default::default()
    })
    .await;
    assert_eq!(seen.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(warnings, Vec::<String>::new());
}

#[tokio::test]
async fn an_abort_that_gets_no_answer_is_given_up_on_and_warned_about() {
    let started = std::time::Instant::now();
    let (err, warnings, seen) = read_failure_mid_upload(Faults {
        abort_hangs: true,
        ..Default::default()
    })
    .await;
    assert_eq!(
        err,
        "blobs: read file for upload: app.bin: simulated read failure"
    );
    assert_eq!(seen.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(
        warnings,
        vec![incomplete_upload_warning(
            &UploadDestination::test_s3(),
            "dist/app.bin",
            "aborting it got no answer within 20ms"
        )]
    );
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[tokio::test]
async fn an_abort_that_goes_through_warns_nothing() {
    let (_, warnings, seen) = read_failure_mid_upload(Faults::default()).await;
    assert_eq!(seen.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(warnings, Vec::<String>::new());
}

/// Azure answers an abort with success having removed nothing, so a failed
/// upload there warns about its staged blocks once, on that answer.
#[tokio::test]
async fn a_failed_azure_upload_warns_once_that_its_blocks_remain() {
    let (store, seen) = Probe::in_memory(Faults::default());
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    let destination = UploadDestination {
        provider: Provider::AzBlob,
        bucket: "container".to_string(),
        endpoint: None,
    };
    let sink = UploadSink {
        destination: destination.clone(),
        ..small_sink(&store, "dist/app.bin")
    };
    let err = stream_upload(
        reader(9 * PART, Some(5 * PART + 100)),
        sink,
        "app.bin",
        "dist/app.bin",
        &log,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "blobs: read file for upload: app.bin: simulated read failure"
    );
    assert_eq!(seen.aborts.load(Ordering::SeqCst), 1);
    assert_eq!(
        captured.warn_messages(),
        vec![incomplete_upload_warning(
            &destination,
            "dist/app.bin",
            "Azure has no abort that removes blocks already staged"
        )]
    );

    // Below one part nothing was staged, so there is nothing to warn about.
    let (store, _) = Probe::in_memory(Faults::default());
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    let sink = UploadSink {
        destination,
        ..small_sink(&store, "dist/app.bin")
    };
    stream_upload(
        reader(PART / 2, Some(10)),
        sink,
        "app.bin",
        "dist/app.bin",
        &log,
    )
    .await
    .unwrap_err();
    assert_eq!(captured.warn_messages(), Vec::<String>::new());
}

/// Below one part no multipart upload exists, so there is nothing to abort
/// and nothing to warn about even against a store whose abort would fail.
#[tokio::test]
async fn a_failure_below_one_part_aborts_nothing() {
    let (store, seen) = Probe::in_memory(Faults {
        abort: Some(Fault::Generic),
        ..Default::default()
    });
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    stream_upload(
        reader(PART / 2, Some(1000)),
        small_sink(&store, "dist/app.bin"),
        "app.bin",
        "dist/app.bin",
        &log,
    )
    .await
    .unwrap_err();
    assert_eq!(seen.aborts.load(Ordering::SeqCst), 0);
    assert_eq!(captured.warn_messages(), Vec::<String>::new());
}

#[tokio::test]
async fn a_multipart_upload_that_cannot_be_completed_warns_that_parts_may_remain() {
    let (store, _) = Probe::in_memory(Faults {
        complete: Some(Fault::Generic),
        ..Default::default()
    });
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    let err = stream_upload(
        reader(3 * PART, None),
        small_sink(&store, "dist/app.bin"),
        "app.bin",
        "dist/app.bin",
        &log,
    )
    .await
    .unwrap_err();
    assert_eq!(
        err.to_string(),
        "blobs: upload failed for app.bin → dist/app.bin: Generic Probe error: injected failure"
    );
    assert_eq!(
        captured.warn_messages(),
        vec![incomplete_upload_warning(
            &UploadDestination::test_s3(),
            "dist/app.bin",
            "completing the upload failed"
        )]
    );
}

/// The warning the blob storage page quotes is the one an abort that gets no
/// answer produces, for the bucket and key the page names. The test's short
/// timeout is the only thing swapped for the production one.
#[tokio::test]
async fn the_warning_quoted_in_the_blob_docs_is_what_an_unanswered_abort_produces() {
    let page = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/site/content/docs/publish/blob-storage.md"
    ))
    .unwrap();
    let quoted: Vec<&str> = page
        .lines()
        .filter_map(|line| line.trim_start().strip_prefix("Warning "))
        .filter(|line| !line.contains('\u{2026}'))
        .collect();

    let (store, _) = Probe::in_memory(Faults {
        abort_hangs: true,
        ..Default::default()
    });
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    let key = "myapp/v1.0.0/myapp.tar.gz";
    let sink = UploadSink {
        abort_timeout: Duration::from_millis(20),
        destination: UploadDestination {
            provider: Provider::S3,
            bucket: "my-release-bucket".to_string(),
            endpoint: None,
        },
        ..small_sink(&store, key)
    };
    stream_upload(
        reader(9 * PART, Some(5 * PART + 100)),
        sink,
        "myapp.tar.gz",
        key,
        &log,
    )
    .await
    .unwrap_err();
    let mut produced: Vec<String> = captured
        .warn_messages()
        .iter()
        .map(|w| w.replace("within 20ms", &format!("within {ABORT_TIMEOUT:?}")))
        .collect();

    // The Azure line: a failed upload whose abort removed nothing.
    let (store, _) = Probe::in_memory(Faults::default());
    let (log, captured) = StageLogger::with_capture("blob-test", Verbosity::Quiet);
    let sink = UploadSink {
        destination: UploadDestination {
            provider: Provider::AzBlob,
            bucket: "my-container".to_string(),
            endpoint: None,
        },
        ..small_sink(&store, key)
    };
    stream_upload(
        reader(9 * PART, Some(5 * PART + 100)),
        sink,
        "myapp.tar.gz",
        key,
        &log,
    )
    .await
    .unwrap_err();
    produced.extend(captured.warn_messages());

    // The first quoted warning is the rollback's, held by
    // `blob_rollback_leaves_an_overwritten_object_in_place` (`publisher.rs`).
    assert_eq!(quoted.len(), 3);
    assert_eq!(produced, quoted[1..]);
}

/// Each provider is told the cleanup it actually offers: a custom S3
/// endpoint is carried into both commands, a key with a space stays one
/// argument, and neither GCS nor Azure is sent to the `aws` CLI.
#[test]
fn the_warning_names_each_providers_own_cleanup() {
    let at = |provider, endpoint: Option<&str>| UploadDestination {
        provider,
        bucket: "rel".to_string(),
        endpoint: endpoint.map(str::to_string),
    };
    assert_eq!(
        incomplete_upload_warning(
            &at(Provider::S3, Some("http://minio:9000")),
            "v1/my app.bin",
            "why"
        ),
        "an incomplete multipart upload may remain at s3://rel/v1/my app.bin (why); its \
         parts are stored and billed until it is aborted — find its upload ID with `aws \
         s3api list-multipart-uploads --bucket 'rel' --prefix 'v1/my app.bin' --endpoint-url \
         'http://minio:9000'` and remove it with `aws s3api abort-multipart-upload --bucket \
         'rel' --key 'v1/my app.bin' --upload-id <UploadId> --endpoint-url \
         'http://minio:9000'`, or set an AbortIncompleteMultipartUpload lifecycle rule on the \
         bucket"
    );
    // A key holding a quote or a `$` is still one shell word in both commands.
    let quoted = incomplete_upload_warning(&at(Provider::S3, None), "v1/it's $HOME.bin", "why");
    assert!(
        quoted.contains(r"--prefix 'v1/it'\''s $HOME.bin'`")
            && quoted.contains(r"--key 'v1/it'\''s $HOME.bin' --upload-id"),
        "{quoted}"
    );
    assert_eq!(
        incomplete_upload_warning(&at(Provider::Gcs, None), "v1/app.bin", "why"),
        "an incomplete multipart upload may remain at gs://rel/v1/app.bin (why); its parts \
         are stored and billed until it is aborted — gcloud has no command that lists or \
         aborts one, so set an AbortIncompleteMultipartUpload lifecycle rule with an `age` \
         condition on the bucket, or abort it through the XML API"
    );
    assert_eq!(
        incomplete_upload_warning(&at(Provider::AzBlob, None), "v1/app.bin", "why"),
        "uncommitted blocks may remain at azblob://rel/v1/app.bin (why); Azure removes them \
         a week after the last block was written, and uploading the file again removes \
         them sooner"
    );
}

// -----------------------------------------------------------------------
// Store errors through the stream
// -----------------------------------------------------------------------

async fn upload_error(total: usize, faults: Faults) -> String {
    let (store, _) = Probe::in_memory(faults);
    stream_upload(
        reader(total, None),
        small_sink(&store, "dist/app.bin"),
        "app.bin",
        "dist/app.bin",
        &quiet(),
    )
    .await
    .unwrap_err()
    .to_string()
}

const NOT_FOUND: &str =
    "blobs: bucket or object not found (dist/app.bin): uploading app.bin → dist/app.bin";
const ACCESS_DENIED: &str =
    "blobs: access denied — check permissions. Uploading app.bin → dist/app.bin (dist/app.bin)";
const UPLOAD_GONE: &str = "blobs: the multipart upload was aborted or expired part way through \
     (dist/app.bin): uploading app.bin → dist/app.bin; run the release again to upload the file";

#[tokio::test]
async fn a_single_put_reports_a_missing_bucket_and_a_refusal_in_their_own_words() {
    let put = |fault| Faults {
        put: Some((fault, "")),
        ..Default::default()
    };
    assert_eq!(upload_error(100, put(Fault::NotFound)).await, NOT_FOUND);
    assert_eq!(
        upload_error(100, put(Fault::PermissionDenied)).await,
        ACCESS_DENIED
    );
}

/// Opening the multipart upload is the first request a large file makes, so
/// a `NotFound` there is still about the bucket.
#[tokio::test]
async fn a_multipart_upload_that_cannot_be_opened_reports_the_bucket() {
    let create = |fault| Faults {
        create: Some(fault),
        ..Default::default()
    };
    assert_eq!(
        upload_error(4 * PART, create(Fault::NotFound)).await,
        NOT_FOUND
    );
    assert_eq!(
        upload_error(4 * PART, create(Fault::PermissionDenied)).await,
        ACCESS_DENIED
    );
}

#[tokio::test]
async fn a_not_found_after_the_upload_opened_reports_the_upload_as_gone() {
    for part in [0, 2, 3] {
        let faults = Faults {
            part: Some((part, Fault::NotFound)),
            ..Default::default()
        };
        assert_eq!(
            upload_error(4 * PART, faults).await,
            UPLOAD_GONE,
            "part {part}"
        );
    }
    let faults = Faults {
        complete: Some(Fault::NotFound),
        ..Default::default()
    };
    assert_eq!(upload_error(4 * PART, faults).await, UPLOAD_GONE);

    // Any other failure of a part keeps its own wording.
    let faults = Faults {
        part: Some((2, Fault::PermissionDenied)),
        ..Default::default()
    };
    assert_eq!(upload_error(4 * PART, faults).await, ACCESS_DENIED);
}

// -----------------------------------------------------------------------
// The identical-object comparison
// -----------------------------------------------------------------------

const RANGE: usize = 1000;

/// A probed store holding `content` at `k`, and that object's metadata.
async fn holding(content: &[u8], faults: Faults) -> (Arc<dyn ObjectStore>, Arc<Seen>, ObjectMeta) {
    let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
    let key = ObjectPath::from("k");
    inner.put(&key, content.to_vec().into()).await.unwrap();
    let meta = inner.head(&key).await.unwrap();
    let (store, seen) = Probe::over(inner, faults);
    (store, seen, meta)
}

#[tokio::test]
async fn an_object_is_identical_only_when_every_byte_was_compared() {
    let content = pattern(5 * RANGE + 3);
    let compare = |local: Vec<u8>| {
        let content = content.clone();
        async move {
            let (store, seen, meta) = holding(&content, Faults::default()).await;
            let verdict =
                object_matches_reader(&store, &meta, std::io::Cursor::new(local), RANGE).await;
            (verdict, seen, meta)
        }
    };

    let (verdict, seen, meta) = compare(content.clone()).await;
    assert_eq!(verdict, Some(true));
    assert_eq!(
        *seen.range_sizes.lock().unwrap(),
        vec![1000, 1000, 1000, 1000, 1000, 3]
    );
    assert!(meta.e_tag.is_some());
    assert!(
        seen.range_if_match
            .lock()
            .unwrap()
            .iter()
            .all(|pin| *pin == meta.e_tag),
        "a range was read without naming the object's version"
    );

    // The local side ends early: at a range boundary, inside a range, at once.
    for kept in [2 * RANGE, 2 * RANGE + 1, 0] {
        let (verdict, _, _) = compare(content[..kept].to_vec()).await;
        assert_eq!(verdict, Some(false), "local side cut to {kept} bytes");
    }

    let mut longer = content.clone();
    longer.push(0);
    assert_eq!(compare(longer).await.0, Some(false));

    let mut changed = content.clone();
    *changed.last_mut().unwrap() ^= 0xff;
    assert_eq!(compare(changed).await.0, Some(false));
}

#[tokio::test]
async fn an_empty_object_is_compared_without_reading_it() {
    let (store, seen, meta) = holding(b"", Faults::default()).await;
    assert_eq!(
        object_matches_reader(&store, &meta, std::io::Cursor::new(Vec::new()), RANGE).await,
        Some(true)
    );
    assert_eq!(
        object_matches_reader(&store, &meta, std::io::Cursor::new(vec![1u8]), RANGE).await,
        Some(false)
    );
    assert_eq!(*seen.range_sizes.lock().unwrap(), Vec::<u64>::new());
}

#[tokio::test]
async fn a_remote_read_that_fails_mid_compare_proves_nothing() {
    let content = pattern(5 * RANGE);
    let faults = Faults {
        range: Some(2),
        ..Default::default()
    };
    let (store, seen, meta) = holding(&content, faults).await;
    assert_eq!(
        object_matches_reader(&store, &meta, std::io::Cursor::new(content), RANGE).await,
        None
    );
    assert_eq!(seen.range_sizes.lock().unwrap().len(), 3);
}

/// The replacement differs from the original only in a range that was
/// already compared, so reading the remaining ranges from it unpinned
/// would find every byte equal.
#[tokio::test]
async fn an_object_replaced_between_two_ranges_is_not_identical() {
    let content = pattern(5 * RANGE);
    let mut replacement = content.clone();
    replacement[0] ^= 0xff;
    let faults = Faults {
        swap_before_range: Some((1, replacement)),
        ..Default::default()
    };
    let (store, seen, meta) = holding(&content, faults).await;
    assert_eq!(
        object_matches_reader(&store, &meta, std::io::Cursor::new(content), RANGE).await,
        Some(false)
    );
    assert_eq!(seen.range_sizes.lock().unwrap().len(), 2);
}

// -----------------------------------------------------------------------
// The whole publish path
// -----------------------------------------------------------------------

/// A file several parts long is handed to the store one part at a time,
/// `UPLOAD_PART_CONCURRENCY` in flight, and never as one payload; the
/// re-run compares it one range at a time and skips it.
#[test]
fn a_file_several_parts_long_is_never_handed_to_the_store_whole() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("big.tar.gz");
    let mut content = pattern(6 * UPLOAD_PART_BYTES + 12345);
    content[..3].copy_from_slice(b"\x1f\x8b\x08");
    std::fs::write(&file, &content).unwrap();

    let (store, seen) = Probe::in_memory(Faults {
        part_delay: Duration::from_millis(30),
        ..Default::default()
    });
    let config = BlobConfig {
        cache_control: Some(vec!["max-age=60".to_string()]),
        ..Default::default()
    };
    let rt = tokio::runtime::Runtime::new().unwrap();
    let files = [(file.clone(), "big.tar.gz")];

    let (first, result) = publish(&rt, &store, &files, &config, &quiet());
    result.unwrap();
    assert_eq!(first.uploaded, vec!["dist/big.tar.gz"]);
    assert!(first.skipped_identical.is_empty());

    assert_eq!(
        *seen.put_sizes.lock().unwrap(),
        Vec::<usize>::new(),
        "the file was sent as a single payload"
    );
    let parts = seen.part_sizes.lock().unwrap().clone();
    assert_eq!(parts.len(), 7);
    assert_eq!(parts.iter().max(), Some(&UPLOAD_PART_BYTES));
    assert_eq!(parts.iter().sum::<usize>(), content.len());
    assert_eq!(
        seen.max_in_flight.load(Ordering::SeqCst),
        UPLOAD_PART_CONCURRENCY
    );

    let sent = seen.multipart_attributes.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(content_type(&sent[0]), Some("application/x-gzip"));
    assert_eq!(
        sent[0].get(&Attribute::CacheControl).map(|v| v.as_ref()),
        Some("max-age=60")
    );
    assert_eq!(
        sent[0]
            .get(&Attribute::ContentDisposition)
            .map(|v| v.as_ref()),
        Some("attachment;filename=big.tar.gz")
    );
    let (bytes, _) = rt.block_on(stored(&store, "dist/big.tar.gz"));
    assert!(bytes == content, "the stored object differs from the file");

    let (second, result) = publish(&rt, &store, &files, &config, &quiet());
    result.unwrap();
    assert!(second.uploaded.is_empty());
    assert_eq!(second.skipped_identical, vec!["dist/big.tar.gz"]);
    let ranges = seen.range_sizes.lock().unwrap().clone();
    assert_eq!(ranges.len(), 7);
    assert_eq!(ranges.iter().max(), Some(&(UPLOAD_PART_BYTES as u64)));
    assert_eq!(ranges.iter().sum::<u64>(), content.len() as u64);
    assert_eq!(seen.part_sizes.lock().unwrap().len(), 7);

    // A same-size change in the last part is seen and uploaded.
    let last = content.len() - 1;
    content[last] ^= 0xff;
    std::fs::write(&file, &content).unwrap();
    let (third, result) = publish(&rt, &store, &files, &config, &quiet());
    result.unwrap();
    assert_eq!(third.uploaded, vec!["dist/big.tar.gz"]);
    let (bytes, _) = rt.block_on(stored(&store, "dist/big.tar.gz"));
    assert!(bytes == content, "the changed file was not re-uploaded");
}

/// One byte below the part size is one PUT carrying the headers; exactly
/// the part size is a multipart upload of one part.
#[test]
fn the_part_size_is_where_a_single_put_becomes_a_multipart_upload() {
    let tmp = tempfile::TempDir::new().unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let config = BlobConfig {
        cache_control: Some(vec!["max-age=60".to_string()]),
        ..Default::default()
    };

    let below = tmp.path().join("below.txt");
    std::fs::write(&below, vec![b'a'; UPLOAD_PART_BYTES - 1]).unwrap();
    let (store, seen) = Probe::in_memory(Faults::default());
    let (_, result) = publish(&rt, &store, &[(below, "below.txt")], &config, &quiet());
    result.unwrap();
    assert_eq!(*seen.put_sizes.lock().unwrap(), vec![UPLOAD_PART_BYTES - 1]);
    assert_eq!(*seen.part_sizes.lock().unwrap(), Vec::<usize>::new());
    let sent = seen.put_attributes.lock().unwrap().clone();
    assert_eq!(content_type(&sent[0]), Some("text/plain; charset=utf-8"));
    assert_eq!(
        sent[0].get(&Attribute::CacheControl).map(|v| v.as_ref()),
        Some("max-age=60")
    );
    assert_eq!(
        sent[0]
            .get(&Attribute::ContentDisposition)
            .map(|v| v.as_ref()),
        Some("attachment;filename=below.txt")
    );

    let exact = tmp.path().join("exact.txt");
    std::fs::write(&exact, vec![b'a'; UPLOAD_PART_BYTES]).unwrap();
    let (store, seen) = Probe::in_memory(Faults::default());
    let (_, result) = publish(&rt, &store, &[(exact, "exact.txt")], &config, &quiet());
    result.unwrap();
    assert_eq!(*seen.put_sizes.lock().unwrap(), Vec::<usize>::new());
    assert_eq!(*seen.part_sizes.lock().unwrap(), vec![UPLOAD_PART_BYTES]);
}

#[test]
fn the_production_sink_uses_the_shared_part_size_and_concurrency() {
    let sink = UploadSink::new(
        Arc::new(InMemory::new()),
        UploadDestination::test_s3(),
        ObjectPath::from("k"),
        Attributes::new(),
    );
    assert_eq!(sink.part_bytes, UPLOAD_PART_BYTES);
    assert_eq!(sink.concurrency, UPLOAD_PART_CONCURRENCY);
    assert_eq!(sink.abort_timeout, Duration::from_secs(30));
}

/// What stands at the key decides between a skip and an upload: a size
/// mismatch uploads without reading the object, and an object replaced or
/// unreadable part way through the comparison is uploaded over.
#[test]
fn an_object_that_cannot_be_proven_identical_is_uploaded_over() {
    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("app.bin");
    let content = pattern(UPLOAD_PART_BYTES + 4096);
    std::fs::write(&file, &content).unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let files = [(file, "app.bin")];
    let key = ObjectPath::from("dist/app.bin");

    let run = |remote: Vec<u8>, faults: Faults| {
        let inner: Arc<dyn ObjectStore> = Arc::new(InMemory::new());
        rt.block_on(inner.put(&key, remote.into())).unwrap();
        let (store, seen) = Probe::over(inner, faults);
        let (report, result) = publish(&rt, &store, &files, &BlobConfig::default(), &quiet());
        result.unwrap();
        let (bytes, _) = rt.block_on(stored(&store, "dist/app.bin"));
        assert!(bytes == content, "the object does not hold the file");
        (report, seen)
    };

    let (report, seen) = run(content.clone(), Faults::default());
    assert_eq!(report.skipped_identical, vec!["dist/app.bin"]);
    assert_eq!(seen.range_sizes.lock().unwrap().len(), 2);

    let (report, seen) = run(content[..100].to_vec(), Faults::default());
    assert_eq!(report.uploaded, vec!["dist/app.bin"]);
    assert_eq!(*seen.range_sizes.lock().unwrap(), Vec::<u64>::new());

    let mut replacement = content.clone();
    replacement[0] ^= 0xff;
    let (report, seen) = run(
        content.clone(),
        Faults {
            swap_before_range: Some((1, replacement)),
            ..Default::default()
        },
    );
    assert_eq!(report.uploaded, vec!["dist/app.bin"]);
    assert_eq!(seen.range_sizes.lock().unwrap().len(), 2);

    let (report, _) = run(
        content.clone(),
        Faults {
            range: Some(1),
            ..Default::default()
        },
    );
    assert_eq!(report.uploaded, vec!["dist/app.bin"]);
}

/// The objects written before and beside a failed file exist in the bucket,
/// so the report that travels with the error names them.
#[test]
fn a_failed_file_still_reports_the_keys_that_were_written() {
    let tmp = tempfile::TempDir::new().unwrap();
    let files: Vec<(std::path::PathBuf, &str)> = ["a.txt", "b.txt", "c.txt"]
        .into_iter()
        .map(|name| {
            let path = tmp.path().join(name);
            std::fs::write(&path, name).unwrap();
            (path, name)
        })
        .collect();
    let (store, _) = Probe::in_memory(Faults {
        put: Some((Fault::PermissionDenied, "c.txt")),
        ..Default::default()
    });
    let rt = tokio::runtime::Runtime::new().unwrap();

    let (report, result) = publish(&rt, &store, &files, &BlobConfig::default(), &quiet());
    assert_eq!(
        result.unwrap_err().to_string(),
        format!(
            "blobs: access denied — check permissions. Uploading {} → dist/c.txt (dist/c.txt)",
            files[2].0.display()
        )
    );
    assert_eq!(report.uploaded, vec!["dist/a.txt", "dist/b.txt"]);
    assert!(report.skipped_identical.is_empty());
}

/// A client-side encrypted object is typed from its ciphertext, the bytes
/// the object holds.
#[cfg(unix)]
#[test]
#[serial_test::serial(path_env)]
fn a_client_side_encrypted_object_is_typed_from_its_ciphertext() {
    use anodizer_core::test_helpers::fake_tool::FakeToolDir;

    let tools = FakeToolDir::new();
    tools.tool("gcloud").stdout("ciphertext").install();
    let _guard = tools.activate();

    let tmp = tempfile::TempDir::new().unwrap();
    let file = tmp.path().join("app.bin");
    std::fs::write(&file, b"\x7fELF\x02\x01\x01\x00").unwrap();
    let (store, seen) = Probe::in_memory(Faults::default());
    let rt = tokio::runtime::Runtime::new().unwrap();
    let ctx = make_ctx();
    let (report, result) = upload_files_owned(
        &rt,
        Arc::clone(&store),
        vec![(file, "app.bin".to_string())],
        "dist".to_string(),
        vec![build_put_options(&BlobConfig::default(), "app.bin", &ctx).unwrap()],
        1,
        Some((
            "gcpkms://projects/p/locations/l/keyRings/r/cryptoKeys/k".to_string(),
            crate::kms::KmsProvider::Gcp,
        )),
        UploadDestination::test_s3(),
        &quiet(),
    );
    result.unwrap();
    assert_eq!(report.uploaded, vec!["dist/app.bin"]);
    let sent = seen.put_attributes.lock().unwrap().clone();
    assert_eq!(content_type(&sent[0]), Some("text/plain; charset=utf-8"));
}
