-- Forward-only Syrup Rail schema upgrade from version 4 to version 5.
--
-- Apply these bytes once inside a host-owned transaction while all schema-v4
-- billing writers are stopped. Version 5 appends six nullable billing-address
-- columns to stored payment methods and payment attempts. Historical rows keep
-- NULL addresses; no address is inferred. Adding the columns is metadata-only,
-- but validating each new CHECK constraint scans its table under the
-- ACCESS EXCLUSIVE lock taken by ALTER TABLE, so rehearse the duration on a
-- production-sized copy. Roll back before commit on any failure; after commit
-- keep writers stopped and roll forward with a schema-v5-aware build.

SET LOCAL lock_timeout = '5s';

DO $$
BEGIN
    IF EXISTS (
        SELECT 1
        FROM information_schema.columns
        WHERE table_schema = 'public'
            AND table_name IN ('billing_payment_methods', 'billing_payment_attempts')
            AND column_name LIKE 'billing\_address\_%' ESCAPE '\'
    )
    THEN
        RAISE EXCEPTION
            'schema-v5 upgrade is already applied or was manually altered; inspect the catalog and do not rerun this artifact';
    END IF;
END
$$;

ALTER TABLE public.billing_payment_methods
    ADD COLUMN billing_address_line1 text,
    ADD COLUMN billing_address_line2 text,
    ADD COLUMN billing_address_city text,
    ADD COLUMN billing_address_region text,
    ADD COLUMN billing_address_postal_code text,
    ADD COLUMN billing_address_country text,
    ADD CONSTRAINT billing_payment_methods_billing_address_valid
        CHECK (
            (
                billing_address_line1 IS NULL
                AND billing_address_line2 IS NULL
                AND billing_address_city IS NULL
                AND billing_address_region IS NULL
                AND billing_address_postal_code IS NULL
                AND billing_address_country IS NULL
            )
            OR (
                billing_address_line1 IS NOT NULL
                AND length(btrim(billing_address_line1)) > 0
                AND octet_length(billing_address_line1) <= 255
                AND billing_address_country IS NOT NULL
                AND billing_address_country ~ '^[A-Z]{2}$'
                AND (
                    billing_address_line2 IS NULL
                    OR (
                        length(btrim(billing_address_line2)) > 0
                        AND octet_length(billing_address_line2) <= 255
                    )
                )
                AND (
                    billing_address_city IS NULL
                    OR (
                        length(btrim(billing_address_city)) > 0
                        AND octet_length(billing_address_city) <= 255
                    )
                )
                AND (
                    billing_address_region IS NULL
                    OR (
                        length(btrim(billing_address_region)) > 0
                        AND octet_length(billing_address_region) <= 255
                    )
                )
                AND (
                    billing_address_postal_code IS NULL
                    OR (
                        length(btrim(billing_address_postal_code)) > 0
                        AND octet_length(billing_address_postal_code) <= 255
                    )
                )
            )
        );

ALTER TABLE public.billing_payment_attempts
    ADD COLUMN billing_address_line1 text,
    ADD COLUMN billing_address_line2 text,
    ADD COLUMN billing_address_city text,
    ADD COLUMN billing_address_region text,
    ADD COLUMN billing_address_postal_code text,
    ADD COLUMN billing_address_country text,
    ADD CONSTRAINT billing_payment_attempts_billing_address_valid
        CHECK (
            (
                billing_address_line1 IS NULL
                AND billing_address_line2 IS NULL
                AND billing_address_city IS NULL
                AND billing_address_region IS NULL
                AND billing_address_postal_code IS NULL
                AND billing_address_country IS NULL
            )
            OR (
                billing_address_line1 IS NOT NULL
                AND length(btrim(billing_address_line1)) > 0
                AND octet_length(billing_address_line1) <= 255
                AND billing_address_country IS NOT NULL
                AND billing_address_country ~ '^[A-Z]{2}$'
                AND (
                    billing_address_line2 IS NULL
                    OR (
                        length(btrim(billing_address_line2)) > 0
                        AND octet_length(billing_address_line2) <= 255
                    )
                )
                AND (
                    billing_address_city IS NULL
                    OR (
                        length(btrim(billing_address_city)) > 0
                        AND octet_length(billing_address_city) <= 255
                    )
                )
                AND (
                    billing_address_region IS NULL
                    OR (
                        length(btrim(billing_address_region)) > 0
                        AND octet_length(billing_address_region) <= 255
                    )
                )
                AND (
                    billing_address_postal_code IS NULL
                    OR (
                        length(btrim(billing_address_postal_code)) > 0
                        AND octet_length(billing_address_postal_code) <= 255
                    )
                )
            )
        );
