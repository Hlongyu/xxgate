-- The previous prefix allowlist mislabeled tool outputs (ctco/fco) and other
-- unfamiliar item types as messages. Repair only generated outbound aliases;
-- keep their UUID suffix, client identifier, binding and reverse-direction
-- mappings unchanged.
UPDATE identity_mappings
SET upstream_id = split_part(client_id, '_', 1) || substring(upstream_id FROM 4)
WHERE kind = 'item'
  AND upstream_id ~ '^msg_[0-9a-f]{32}$'
  AND client_id ~ '^[A-Za-z0-9]{1,64}_'
  AND split_part(client_id, '_', 1) NOT IN (
      'msg', 'rs', 'fc', 'ctc', 'ig', 'ws', 'at', 'cmp', 'compaction'
  );
