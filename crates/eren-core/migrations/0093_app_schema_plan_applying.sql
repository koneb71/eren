-- A schema plan being applied says so.
--
-- Applying read the pending plan, ran its DDL and then wrote the outcome, with
-- nothing between the read and the write to say the plan was taken. Two
-- clicks — the app page and the inbox, or one impatient person — both read it
-- as pending and both ran it. `apply_plan` now claims the plan first
-- (`pending` → `applying`, in the WHERE of the write) and records the outcome
-- only against its own claim.
ALTER TABLE app_schema_plans DROP CONSTRAINT IF EXISTS app_schema_plans_status_known;
ALTER TABLE app_schema_plans ADD CONSTRAINT app_schema_plans_status_known
    CHECK (status IN ('pending', 'applying', 'applied', 'discarded', 'failed'));
