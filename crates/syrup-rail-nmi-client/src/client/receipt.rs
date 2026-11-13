use super::*;
use crate::{TransactionReceipt, TransactionReceiptLookup, TransactionReceiptQuery};

impl Client {
    /// One read-only exact query with the standard response/transport bounds.
    /// Both selectors must be echoed consistently by exactly one transaction.
    /// The action history is bounded to 100 entries. No current-state approval
    /// test is imposed: a later refund/void does not erase the original sale.
    pub async fn query_transaction_receipt(
        &self,
        request: TransactionReceiptQuery,
    ) -> Result<Option<TransactionReceipt>, QueryError> {
        let query = receipt_query(request)?;
        let text = self.query_transaction_text(&query).await?;
        response::xml::receipt::parse(&text, &query).map_err(WireError::into_query)
    }

    /// Opts into normalized receipt evidence and payload-free bound conflicts.
    /// Uses the same exact selectors and bounded, read-only transport as
    /// [`Self::query_transaction_receipt`]. Invalid or ambiguous identities
    /// remain errors. A proven conflict survives unrelated malformed fields.
    /// Otherwise missing currency or malformed bound fields yield `Incomplete`,
    /// preserving individually trustworthy facts for comparison, not completion.
    pub async fn query_transaction_receipt_evidence(
        &self,
        request: TransactionReceiptQuery,
    ) -> Result<TransactionReceiptLookup, QueryError> {
        let query = receipt_query(request)?;
        let text = self.query_transaction_text(&query).await?;
        response::xml::receipt::parse_evidence(&text, &query).map_err(WireError::into_query)
    }
}

fn receipt_query(request: TransactionReceiptQuery) -> Result<TransactionQuery, QueryError> {
    if request.transaction_id.trim().is_empty() || request.order_id.trim().is_empty() {
        return Err(QueryError::InvalidRequest(
            "receipt transaction and order identifiers are required".into(),
        ));
    }
    Ok(TransactionQuery {
        transaction_id: Some(request.transaction_id),
        order_id: Some(request.order_id),
    })
}
