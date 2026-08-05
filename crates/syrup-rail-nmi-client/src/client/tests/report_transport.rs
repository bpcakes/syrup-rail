use super::*;

#[test]
fn report_query_uses_zero_based_pages_and_rejects_negative_pages() {
    let valid = ReportQuery {
        start_date: "20260701000000".to_owned(),
        end_date: "20260702000000".to_owned(),
        result_limit: 100,
        page_number: 0,
    };
    assert!(validate_report_query(&valid, "query_key").is_ok());

    let negative_page = ReportQuery {
        page_number: -1,
        ..valid.clone()
    };
    assert!(matches!(
        validate_report_query(&negative_page, "query_key"),
        Err(QueryError::InvalidRequest(_))
    ));

    for result_limit in [0, 101] {
        let invalid_limit = ReportQuery {
            result_limit,
            ..valid.clone()
        };
        assert!(matches!(
            validate_report_query(&invalid_limit, "query_key"),
            Err(QueryError::InvalidRequest(_))
        ));
    }
}

#[tokio::test]
async fn full_report_page_can_exceed_the_standard_response_limit() {
    let response_text = "x".repeat(3 * 1024);
    let transactions = (0..MAX_NMI_TRANSACTION_REPORTS)
        .map(|index| {
            format!(
                "<transaction><transaction_id>txn_report_{index:03}</transaction_id><order_id>ck_report_{index:03}</order_id><condition>complete</condition><action><action_type>settle</action_type><response_text>{response_text}</response_text></action></transaction>"
            )
        })
        .collect::<String>();
    let response = format!("<nm_response>{transactions}</nm_response>");
    assert!(response.len() > MAX_NMI_STANDARD_RESPONSE_BYTES);
    assert!(response.len() < MAX_NMI_REPORT_RESPONSE_BYTES);
    let (client, request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/xml", response.into_bytes()).await;

    let reports = client
        .query_transaction_reports(ReportQuery {
            start_date: "20260701000000".to_owned(),
            end_date: "20260702000000".to_owned(),
            result_limit: MAX_NMI_TRANSACTION_REPORTS as i64,
            page_number: 0,
        })
        .await
        .expect("a bounded full report page should parse");

    assert_eq!(reports.len(), MAX_NMI_TRANSACTION_REPORTS);
    assert_eq!(
        reports[0]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_report_000")
    );
    assert_eq!(
        reports[MAX_NMI_TRANSACTION_REPORTS - 1]
            .transaction_id
            .as_ref()
            .map(SensitiveText::expose),
        Some("txn_report_099")
    );
    let request = request_receiver.await.expect("request should be captured");
    let (_, body) = request
        .split_once("\r\n\r\n")
        .expect("captured request should contain a body");
    let fields: std::collections::HashMap<_, _> = form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect();
    assert_eq!(fields.get("result_limit").map(String::as_str), Some("100"));
    assert_eq!(fields.get("page_number").map(String::as_str), Some("0"));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn report_response_still_has_a_finite_transport_bound() {
    let (client, _request_receiver, server) = spawn_capturing_server(
        "HTTP/1.1 200 OK",
        "text/xml",
        vec![b'x'; MAX_NMI_REPORT_RESPONSE_BYTES + 1],
    )
    .await;

    let error = client
        .query_transaction_reports(ReportQuery {
            start_date: "20260701000000".to_owned(),
            end_date: "20260702000000".to_owned(),
            result_limit: MAX_NMI_TRANSACTION_REPORTS as i64,
            page_number: 0,
        })
        .await
        .expect_err("an oversized report response must fail closed");

    assert!(matches!(error, QueryError::Unavailable(_)));
    server.await.expect("server task should finish");
}

#[tokio::test]
async fn report_response_cannot_exceed_the_requested_page_size() {
    let response = br#"<nm_response>
        <transaction><transaction_id>txn_first</transaction_id></transaction>
        <transaction><transaction_id>txn_second</transaction_id></transaction>
    </nm_response>"#
        .to_vec();
    let (client, _request_receiver, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/xml", response).await;

    let error = client
        .query_transaction_reports(ReportQuery {
            start_date: "20260701000000".to_owned(),
            end_date: "20260702000000".to_owned(),
            result_limit: 1,
            page_number: 0,
        })
        .await
        .expect_err("a provider page larger than requested must fail closed");

    assert!(matches!(error, QueryError::MalformedResponse(_)));
    server.await.expect("server task should finish");
}
