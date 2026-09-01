use std::{sync::Arc, time::Duration};

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::*;
use crate::ClientFactory;
use crate::configuration::MAX_NMI_CONCURRENT_REPORTS;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const NO_SUBMISSION_WINDOW: Duration = Duration::from_millis(100);

fn report_query() -> ReportQuery {
    ReportQuery {
        start_date: "20260801".to_owned(),
        end_date: "20260802".to_owned(),
        result_limit: 100,
        page_number: 0,
    }
}

fn account_client(factory: &ClientFactory, endpoint: &Endpoint, ordinal: usize) -> Client {
    factory
        .client_with_duplicate_check(
            endpoint.clone(),
            Credentials::new(format!("private_{ordinal}"), format!("query_{ordinal}"))
                .expect("test credentials should validate"),
            DuplicateCheck::ProcessorConfigured,
        )
        .expect("loopback client should construct")
}

async fn read_request(stream: &mut tokio::net::TcpStream) {
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).await.expect("request should read");
        assert!(read > 0, "request closed before headers");
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break end + 4;
        }
    };
    let content_length = {
        let headers = String::from_utf8_lossy(&buffer[..header_end]);
        headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().ok())
                    .flatten()
            })
            .unwrap_or(0)
    };
    while buffer.len() < header_end + content_length {
        let read = stream
            .read(&mut chunk)
            .await
            .expect("request body should read");
        assert!(read > 0, "request closed before body");
        buffer.extend_from_slice(&chunk[..read]);
    }
}

async fn respond(stream: &mut tokio::net::TcpStream, body: &[u8]) {
    let headers = format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/xml\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(headers.as_bytes())
        .await
        .expect("response headers should write");
    stream
        .write_all(body)
        .await
        .expect("response body should write");
}

#[tokio::test]
async fn factory_clones_share_report_admission_and_shed_excess_work_before_submission() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let endpoint = Endpoint::parse_loopback_http(format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    ))
    .expect("endpoint should validate");
    let factory = ClientFactory::new_with_loopback_http().expect("factory should construct");
    let cloned_factory = factory.clone();
    let client = account_client(&cloned_factory, &endpoint, 0);
    assert!(Arc::ptr_eq(
        &factory.report_admission,
        &client.report_admission
    ));

    let mut permits = Vec::with_capacity(MAX_NMI_CONCURRENT_REPORTS);
    for _ in 0..MAX_NMI_CONCURRENT_REPORTS {
        permits.push(
            factory
                .report_admission
                .clone()
                .acquire_owned()
                .await
                .expect("report admission should remain open"),
        );
    }

    let error = tokio::time::timeout(
        NO_SUBMISSION_WINDOW,
        client.query_transaction_reports(report_query()),
    )
    .await
    .expect("a saturated report query must fail without waiting")
    .expect_err("a saturated report query must fail locally");
    assert!(matches!(error, QueryError::Unavailable(_)));
    assert!(
        tokio::time::timeout(NO_SUBMISSION_WINDOW, listener.accept())
            .await
            .is_err(),
        "a report rejected by local admission must not submit"
    );

    drop(permits.pop());
    let survivor = {
        let client = account_client(&factory, &endpoint, 1);
        tokio::spawn(async move { client.query_transaction_reports(report_query()).await })
    };
    let (mut stream, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("admitted report should connect")
        .expect("report connection should accept");
    read_request(&mut stream).await;
    respond(&mut stream, b"<nm_response></nm_response>").await;
    assert!(
        survivor
            .await
            .expect("report task should finish")
            .expect("report should parse")
            .is_empty()
    );
    drop(permits);
}

#[tokio::test]
async fn ordinary_queries_do_not_wait_for_report_capacity() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("test listener should bind");
    let endpoint = Endpoint::parse_loopback_http(format!(
        "http://{}",
        listener.local_addr().expect("listener address")
    ))
    .expect("endpoint should validate");
    let factory = ClientFactory::new_with_loopback_http().expect("factory should construct");
    let mut permits = Vec::with_capacity(MAX_NMI_CONCURRENT_REPORTS);
    for _ in 0..MAX_NMI_CONCURRENT_REPORTS {
        permits.push(
            factory
                .report_admission
                .clone()
                .acquire_owned()
                .await
                .expect("report admission should remain open"),
        );
    }

    let query = {
        let client = account_client(&factory, &endpoint, 0);
        tokio::spawn(async move { client.account_mode().await })
    };
    let (mut stream, _) = tokio::time::timeout(TEST_TIMEOUT, listener.accept())
        .await
        .expect("ordinary query should bypass full report admission")
        .expect("query connection should accept");
    read_request(&mut stream).await;
    respond(
        &mut stream,
        b"<nm_response><test_mode_enabled>false</test_mode_enabled></nm_response>",
    )
    .await;
    assert_eq!(
        query
            .await
            .expect("account-mode task should finish")
            .expect("account-mode query should succeed"),
        AccountMode::Live
    );
    drop(permits);
}

#[tokio::test(flavor = "current_thread")]
async fn blocking_report_parse_keeps_admission_after_awaiting_task_is_cancelled() {
    let runtime_thread = std::thread::current().id();
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = admission
        .clone()
        .acquire_owned()
        .await
        .expect("test admission should remain open");
    let (started_sender, started_receiver) = tokio::sync::oneshot::channel();
    let (release_sender, release_receiver) = std::sync::mpsc::channel();
    let (finished_sender, finished_receiver) = tokio::sync::oneshot::channel();
    let parse = tokio::spawn(run_blocking_report_parse(permit, move || {
        started_sender
            .send(std::thread::current().id())
            .expect("test should await parser start");
        release_receiver
            .recv()
            .expect("test should release the blocking parser");
        finished_sender
            .send(())
            .expect("test should observe parser completion");
        Ok(())
    }));

    let parser_thread = tokio::time::timeout(TEST_TIMEOUT, started_receiver)
        .await
        .expect("blocking parser should start")
        .expect("blocking parser should report its thread");
    assert_ne!(
        parser_thread, runtime_thread,
        "report parsing must not monopolize an async runtime worker"
    );
    assert!(
        admission.clone().try_acquire_owned().is_err(),
        "an active blocking parse must retain report admission"
    );

    parse.abort();
    assert!(
        parse
            .await
            .expect_err("awaiting task should cancel")
            .is_cancelled()
    );
    assert!(
        admission.clone().try_acquire_owned().is_err(),
        "cancelling the awaiter must not release a detached parser's permit"
    );
    release_sender
        .send(())
        .expect("blocking parser should remain alive until released");
    tokio::time::timeout(TEST_TIMEOUT, finished_receiver)
        .await
        .expect("blocking parser should finish")
        .expect("blocking parser should report completion");
    let recovered = tokio::time::timeout(TEST_TIMEOUT, admission.acquire_owned())
        .await
        .expect("parser completion should release capacity")
        .expect("test admission should remain open");
    drop(recovered);
}

#[tokio::test]
async fn report_parser_panic_propagates_and_releases_admission() {
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = admission
        .clone()
        .acquire_owned()
        .await
        .expect("test admission should remain open");
    let parser = tokio::spawn(run_blocking_report_parse::<(), _>(permit, || {
        panic!("parser panic")
    }));
    let error = parser
        .await
        .expect_err("parser panic must propagate through the awaiting task");
    assert!(error.is_panic());
    let recovered = admission
        .try_acquire_owned()
        .expect("unwinding the parser must release report admission");
    drop(recovered);
}

#[tokio::test]
async fn cancelled_report_parser_is_unavailable_and_releases_admission() {
    let admission = Arc::new(tokio::sync::Semaphore::new(1));
    let permit = admission
        .clone()
        .acquire_owned()
        .await
        .expect("test admission should remain open");
    let parser = tokio::spawn(async move {
        let _admission = permit;
        std::future::pending::<Result<(), WireError>>().await
    });
    parser.abort();
    let error = finish_blocking_report_parse(parser.await)
        .expect_err("blocking-task cancellation must be observed");
    assert!(matches!(error.into_query(), QueryError::Unavailable(_)));
    let recovered = admission
        .try_acquire_owned()
        .expect("cancelling the parser task must release report admission");
    drop(recovered);
}
