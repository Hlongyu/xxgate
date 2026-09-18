-- Encrypted conversation content is forwarded without local provenance tracking.
-- Keep identifier mappings; discard content fingerprints from earlier versions.
DELETE FROM identity_mappings WHERE kind = 'opaque';
