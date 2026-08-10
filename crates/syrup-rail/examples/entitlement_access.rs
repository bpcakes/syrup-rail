use syrup_rail::Entitlement;

/// Applies Syrup Rail's subscription-entitlement policy to a product-access
/// decision. This does not authenticate or authorize the host's subject.
pub const fn permits_product_access(entitlement: &Entitlement) -> bool {
    entitlement.permits_product_access()
}

fn main() {}
