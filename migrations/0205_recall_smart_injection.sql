-- Smart injection (module recall): the lean block at session start and, on the first prompt, the few memories the
-- prompt matches. 'smart' is a user default (user_settings), a context cache variant and a logged injection mode.
ALTER TABLE user_settings DROP CONSTRAINT IF EXISTS user_settings_injection_mode_check;
ALTER TABLE user_settings
  ADD CONSTRAINT user_settings_injection_mode_check CHECK (injection_mode IN ('full', 'lean', 'smart'));
ALTER TABLE retrieval_log DROP CONSTRAINT IF EXISTS retrieval_log_injection_check;
ALTER TABLE retrieval_log
  ADD CONSTRAINT retrieval_log_injection_check CHECK (injection IN ('full', 'lean', 'smart'));
ALTER TABLE context_cache DROP CONSTRAINT IF EXISTS context_cache_mode_check;
ALTER TABLE context_cache ADD CONSTRAINT context_cache_mode_check CHECK (mode IN ('full', 'lean', 'smart'));
