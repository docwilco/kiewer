DELETE FROM session WHERE expires_at <= now()
