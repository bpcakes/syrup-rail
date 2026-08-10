use syrup_rail::{Entitlement, PastDueAccess};

/// Converts Syrup Rail's closed entitlement result into a host product-access
/// decision without treating every payment-state transition as suspension.
pub const fn permits_product_access(entitlement: &Entitlement) -> bool {
    match entitlement {
        Entitlement::PaidActive { .. }
        | Entitlement::PaidThroughCancellation { .. }
        | Entitlement::Granted { .. }
        | Entitlement::PastDue {
            access: PastDueAccess::AllowedDuringDunning,
            ..
        } => true,
        Entitlement::Missing { .. }
        | Entitlement::PastDue {
            access: PastDueAccess::Suspended,
            ..
        } => false,
    }
}

fn main() {}
