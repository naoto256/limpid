// Static PropertySpec names across production module, TLS, queue and global schemas.
// test/highlight.test.mjs checks this inventory against the Rust sources.
// User-defined map keys (headers, TLS profile names, table names) are not reserved.
export const configurationKeys =
  `acks backoff batch_level batch_size batch_timeout bind body_limit brokers ca capacity cert compress compression content_type database endpoint error_log error_log_fallback expected_peer_uid framing group headers host initial_wait key load match max max_attempts max_concurrent_requests max_connections max_hops max_size max_wait mechanism method mode node_id owner password_file path peer peers poll_interval port protocol pubkey queue queue_timeout rate_limit request_rate_limit retry sasl socket state_file tls topic ttl type url username verify`.split(
    " ",
  );
