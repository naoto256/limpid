#![cfg(windows)]

#[cfg(test)]
mod tests {
    use rdkafka::config::ClientConfig;
    use rdkafka::consumer::{BaseConsumer, Consumer};
    use rdkafka::mocking::MockCluster;
    use rdkafka::producer::{BaseProducer, BaseRecord, Producer};
    use rdkafka::{Message, Offset, TopicPartitionList};
    use std::time::Duration;

    #[test]
    fn native_library_retains_tls_plain_and_scram_capabilities() {
        // librdkafka rejects requested builtin.features which were omitted at
        // build time. Avoid NativeClientConfig::get in rdkafka 0.36.2: it assumes
        // the sizing call gives the exact C string length and can panic on the
        // padded buffer returned for this flags property by the locked sys lib.
        ClientConfig::new()
            .set("builtin.features", "ssl,sasl_plain,sasl_scram")
            .create_native_config()
            .expect("native library must retain SSL, PLAIN and SCRAM");
    }

    #[test]
    fn native_mock_broker_receives_exact_payload() {
        // librdkafka's mock cluster listens locally on ephemeral ports. It
        // needs neither an external Kafka service nor user credentials.
        let cluster = MockCluster::new(1).unwrap();
        cluster
            .create_topic("limpid-native-contract", 1, 1)
            .unwrap();
        let bootstrap = cluster.bootstrap_servers();
        let producer: BaseProducer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap)
            .set("message.timeout.ms", "5000")
            .set("log_level", "0")
            .create()
            .unwrap();
        let payload: &[u8] = b"native\x00\xff\n";
        producer
            .send(
                BaseRecord::to("limpid-native-contract")
                    .partition(0)
                    .payload(payload)
                    .key("key"),
            )
            .unwrap();
        producer.flush(Duration::from_secs(10)).unwrap();
        let consumer: BaseConsumer = ClientConfig::new()
            .set("bootstrap.servers", &bootstrap)
            .set("group.id", "limpid-native-contract")
            .set("enable.auto.commit", "false")
            .set("log_level", "0")
            .create()
            .unwrap();
        let mut assignment = TopicPartitionList::new();
        assignment
            .add_partition_offset("limpid-native-contract", 0, Offset::Beginning)
            .unwrap();
        consumer.assign(&assignment).unwrap();
        let record = consumer
            .poll(Duration::from_secs(10))
            .expect("mock broker must return the produced record within the deadline")
            .unwrap();
        assert_eq!(record.payload(), Some(payload));
        assert_eq!(record.key(), Some(b"key".as_slice()));
    }
}
