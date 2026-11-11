use super::*;
use crate::{TransactionReceipt, TransactionReceiptQuery};

impl Client {
    /// One read-only exact query with the standard response/transport bounds.
    /// Both selectors must be echoed consistently by exactly one transaction.
    /// The action history is bounded to 100 entries. No current-state approval
    /// test is imposed: a later refund/void does not erase the original sale.
    pub async fn query_transaction_receipt(
        &self,
        request: TransactionReceiptQuery,
    ) -> Result<Option<TransactionReceipt>, QueryError> {
        if request.transaction_id.trim().is_empty() || request.order_id.trim().is_empty() {
            return Err(QueryError::InvalidRequest(
                "receipt transaction and order identifiers are required".into(),
            ));
        }
        let query = TransactionQuery {
            transaction_id: Some(request.transaction_id),
            order_id: Some(request.order_id),
        };
        let text = self.query_transaction_text(&query).await?;
        response::xml::receipt::parse(&text, &query).map_err(WireError::into_query)
    }
}
