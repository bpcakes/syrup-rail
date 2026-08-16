-- Forward-only Syrup Rail schema upgrade from version 2 to version 3.
--
-- Apply these bytes once inside a host-owned transaction while all schema-v2
-- billing writers are stopped. Version 2 retained only a combined attempt
-- billing name. The rename below preserves that exact display value as the
-- canonical first-name component for historical rows; their last-name
-- component is unknown and therefore remains NULL. New version-3 attempts
-- persist normalized first and last names separately so replay identity is
-- lossless at the provider boundary.

ALTER TABLE public.billing_payment_attempts
    RENAME COLUMN billing_name TO billing_first_name;

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN billing_last_name text;
