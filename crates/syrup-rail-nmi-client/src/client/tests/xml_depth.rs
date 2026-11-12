use super::*;
use crate::{TransactionDiagnosticsQuery, TransactionReceiptQuery};

const CHILD_CASE: &str = "SYRUP_RAIL_XML_DEPTH_CASE";
const TEST_NAME: &str = "client::tests::xml_depth::deep_xml_survives_public_query_boundaries";

#[test]
fn deep_xml_survives_public_query_boundaries() {
    if let Ok(case) = std::env::var(CHILD_CASE) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(query_nested_xml(&case));
        return;
    }
    // An old recursive tokenizer aborts rather than unwinds. Run each public
    // boundary in a separate process with a normal 2 MiB test-thread stack and
    // core dumps disabled so regressions fail without killing the suite.
    for case in [
        "receipt",
        "receipt_evidence",
        "financial",
        "metadata",
        "diagnostics",
        "report",
    ] {
        let output = std::process::Command::new("sh")
            .args(["-c", "ulimit -c 0 && exec \"$@\"", "xml-depth-child"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", TEST_NAME, "--nocapture"])
            .env(CHILD_CASE, case)
            .env("RUST_MIN_STACK", "2097152")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case} child failed: {:?}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

async fn query_nested_xml(case: &str) {
    let nested = format!(
        "<nm_response>{}<transaction><transaction_id>private_txn</transaction_id><order_id>private_order</order_id></transaction>{}</nm_response>",
        "<a>".repeat(35_000),
        "</a>".repeat(35_000),
    );
    assert!(nested.len() < MAX_NMI_STANDARD_RESPONSE_BYTES);
    let (client, requests, server) =
        spawn_capturing_server("HTTP/1.1 200 OK", "text/xml", nested.into_bytes()).await;
    let query = TransactionQuery {
        transaction_id: Some("private_txn".to_owned()),
        order_id: Some("private_order".to_owned()),
    };
    let receipt = TransactionReceiptQuery {
        transaction_id: "private_txn".to_owned(),
        order_id: "private_order".to_owned(),
    };
    let result = match case {
        "receipt" => client.query_transaction_receipt(receipt).await.map(|_| ()),
        "receipt_evidence" => client
            .query_transaction_receipt_evidence(receipt)
            .await
            .map(|_| ()),
        "financial" => client.query_transaction(query).await.map(|_| ()),
        "metadata" => client
            .query_payment_method_metadata(query)
            .await
            .map(|_| ()),
        "diagnostics" => client
            .query_transaction_diagnostics(TransactionDiagnosticsQuery {
                transaction_id: "private_txn".to_owned(),
            })
            .await
            .map(|_| ()),
        "report" => client
            .query_transaction_reports(ReportQuery {
                start_date: "20260919000000".to_owned(),
                end_date: "20260920000000".to_owned(),
                result_limit: 100,
                page_number: 0,
            })
            .await
            .map(|_| ()),
        _ => panic!("unknown XML depth case"),
    };
    let error = result.expect_err("nested transactions must fail structural validation");
    assert!(matches!(error, QueryError::MalformedResponse(_)));
    let formatted = format!("{error:?} {error}");
    for private in ["private_txn", "private_order", "<nm_response>"] {
        assert!(!formatted.contains(private));
    }
    let request = requests.await.unwrap();
    assert!(request.starts_with("POST /api/query.php"));
    server.await.unwrap();
}
