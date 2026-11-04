DELETE FROM account
WHERE handle IS NULL AND created_at <= now() - make_interval(hours => $1)
