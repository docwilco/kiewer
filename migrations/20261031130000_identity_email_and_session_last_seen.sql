-- Two columns for the second round of sign-in (Discord and Google,
-- provider linking, the sessions page). Both additive: the earlier code
-- runs unchanged against this schema, so a rollback of the binary needs
-- no down step.

-- The email the provider vouched for at the last sign-in (Discord's
-- `verified`, Google's `email_verified`, GitHub's public profile), or NULL.
-- Shown in settings next to the login; never a key: two identities with
-- one email are two identities, and an account is found by
-- (provider, provider_user_id) alone (docs/v1-scope.md, Accounts and
-- access: no merge by email).
ALTER TABLE identity ADD COLUMN email text;

-- When the session was last used, touched at most once every few minutes
-- (kiewer_lib::session), for the sessions page. Existing rows start at the
-- migration's time, the closest thing known.
ALTER TABLE session ADD COLUMN last_seen_at timestamptz NOT NULL DEFAULT now();
