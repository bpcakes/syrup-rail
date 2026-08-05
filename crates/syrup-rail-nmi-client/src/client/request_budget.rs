use url::form_urlencoded;

use super::{MAX_NMI_FIXED_REQUEST_BYTES, MAX_NMI_OUTBOUND_REQUEST_BYTES};

#[derive(Clone, Copy)]
pub(super) enum RequestEncoding {
    Form,
    Json,
}

#[derive(Default)]
pub(super) struct OutboundRequestBudget {
    encoded_bytes: usize,
}

impl OutboundRequestBudget {
    pub(super) fn new() -> Result<Self, &'static str> {
        let mut budget = Self::default();
        // This cap covers the complete outbound request envelope, not only
        // the body. The reserve covers JSON/form structure, field names, and
        // fixed literals. Credentials and caller-controlled values are added
        // separately using their worst-case wire encoding, including a JSON
        // request's Authorization-header credential.
        budget.add_bytes(MAX_NMI_FIXED_REQUEST_BYTES)?;
        Ok(budget)
    }

    fn add_encoded_value(
        &mut self,
        encoding: RequestEncoding,
        value: &str,
    ) -> Result<(), &'static str> {
        let bytes = match encoding {
            RequestEncoding::Form => encoded_form_component_bytes(value),
            RequestEncoding::Json => encoded_json_string_bytes(value),
        };
        self.add_bytes(bytes)
    }

    pub(super) fn add_form_pair(&mut self, key: &str, value: &str) -> Result<(), &'static str> {
        self.add_encoded_value(RequestEncoding::Form, key)?;
        self.add_encoded_value(RequestEncoding::Form, value)
    }

    fn add_bytes(&mut self, bytes: usize) -> Result<(), &'static str> {
        self.encoded_bytes = self
            .encoded_bytes
            .checked_add(bytes)
            .ok_or("NMI outbound request exceeds the supported size")?;
        if self.encoded_bytes > MAX_NMI_OUTBOUND_REQUEST_BYTES {
            return Err("NMI outbound request exceeds the supported size");
        }
        Ok(())
    }
}

pub(super) struct OutboundRequestValidator {
    encoding: RequestEncoding,
    budget: OutboundRequestBudget,
}

impl OutboundRequestValidator {
    pub(super) fn new(encoding: RequestEncoding, credential: &str) -> Result<Self, &'static str> {
        let mut budget = OutboundRequestBudget::new()?;
        match encoding {
            RequestEncoding::Form => {
                budget.add_encoded_value(RequestEncoding::Form, credential)?;
            }
            RequestEncoding::Json => budget.add_bytes(credential.len())?,
        }
        Ok(Self { encoding, budget })
    }

    pub(super) fn field(
        &mut self,
        value: &str,
        max_bytes: usize,
        error: &'static str,
    ) -> Result<(), &'static str> {
        ensure_bounded_request_field(value, max_bytes, error)?;
        self.budget.add_encoded_value(self.encoding, value)
    }

    pub(super) fn trimmed_field(
        &mut self,
        value: &str,
        max_bytes: usize,
        error: &'static str,
    ) -> Result<(), &'static str> {
        ensure_bounded_request_field(value, max_bytes, error)?;
        let value = value.trim();
        if value.is_empty() {
            return Ok(());
        }
        self.budget.add_encoded_value(self.encoding, value)
    }
}

fn encoded_form_component_bytes(value: &str) -> usize {
    form_urlencoded::byte_serialize(value.as_bytes())
        .map(str::len)
        .sum()
}

fn encoded_json_string_bytes(value: &str) -> usize {
    value.chars().fold(2, |bytes, character| {
        bytes
            + match character {
                '"' | '\\' | '\u{0008}' | '\u{000c}' | '\n' | '\r' | '\t' => 2,
                '\u{0000}'..='\u{001f}' => 6,
                _ => character.len_utf8(),
            }
    })
}

fn ensure_bounded_request_field(
    value: &str,
    max_bytes: usize,
    error: &'static str,
) -> Result<(), &'static str> {
    if value.len() > max_bytes {
        return Err(error);
    }
    Ok(())
}
