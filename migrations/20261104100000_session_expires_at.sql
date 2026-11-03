-- The collector's sweep of expired sessions (#31; kiewer_lib::collector):
-- a site-wide DELETE ... WHERE expires_at <= now() reads this index as a
-- range, where without it every night would scan the whole table. Before
-- this, a dead row went only when its account next signed in. Additive:
-- the earlier code runs unchanged against this schema.
CREATE INDEX session_expires_at ON session (expires_at);
