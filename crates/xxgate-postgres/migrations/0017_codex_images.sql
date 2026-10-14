-- Codex's Images API supports this model independently of /models discovery.
-- Preserve any existing administrator configuration and do not invent prices.
INSERT INTO model_catalog (id, version, data)
VALUES ('gpt-image-2', 1, '{"id":"gpt-image-2","version":1,"enabled":true,"upstream":{"provider":"openai","access_kind":"codex_oauth","model":"gpt-image-2"},"capabilities":{"image_input":true,"image_generation":true,"tools":false,"reasoning":false,"fast":false}}'::jsonb)
ON CONFLICT (id) DO NOTHING;
