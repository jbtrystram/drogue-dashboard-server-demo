//use bson::{Bson, Document};
use cloudevents::{
    binding::rdkafka::MessageExt, event::ExtensionValue, AttributesReader, Data, Event,
};
use config::{Config, Environment};
use futures_util::stream::StreamExt;
use indexmap::IndexMap;

use rdkafka::{
    config::FromClientConfig,
    consumer::{CommitMode, Consumer, StreamConsumer},
    util::DefaultRuntime,
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::HashMap;

use lazy_static::lazy_static;
use prometheus::{
    IntCounter,
    register_int_counter
};

#[derive(Clone, Debug, Deserialize)]
struct ApplicationConfig {
    pub kafka: KafkaClient,
}

#[derive(Clone, Debug, Deserialize)]
struct KafkaClient {
    pub bootstrap_servers: String,
    #[serde(default)]
    pub properties: HashMap<String, String>,
    pub topic: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Feature {
    pub properties: IndexMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ThingState {
    pub device: String,
    pub revision: u64,
    pub features: IndexMap<String, Feature>,
}

struct Processor {
    consumer: StreamConsumer
}

struct Metrics {
    devices: Vec<String>,
}

lazy_static! {
    static ref TOTAL_EVENTS: IntCounter =
        register_int_counter!("total events", "the total number of events processed by drogue cloud").unwrap();
    static ref DEVICES_SEEN: IntCounter =
        register_int_counter!("devices", "the number of uniques devices that sent messages").unwrap();
}


impl Processor {
    pub async fn new(config: ApplicationConfig) -> anyhow::Result<Self> {
        // kafka

        let mut kafka_config = rdkafka::ClientConfig::new();

        kafka_config.set("bootstrap.servers", config.kafka.bootstrap_servers);
        kafka_config.extend(
            config
                .kafka
                .properties
                .into_iter()
                .map(|(k, v)| (k.replace('_', "."), v)),
        );

        let consumer = StreamConsumer::<_, DefaultRuntime>::from_config(&kafka_config)?;
        consumer.subscribe(&[&config.kafka.topic])?;

        Ok(Self { consumer})
    }

    pub async fn run(self, metrics: &mut Metrics) {
        let mut stream = self.consumer.stream();

        log::info!("Running stream...");

        loop {
            match stream.next().await.map(|r| {
                r.map_err::<anyhow::Error, _>(|err| err.into())
                    .and_then(|msg| {
                        msg.to_event()
                            .map_err(|err| err.into())
                            .map(|evt| (msg, evt))
                    })
            }) {
                None => break,
                Some(Ok(msg)) => match self.handle(msg.1, metrics).await {
                    Ok(_) => {
                        if let Err(err) = self.consumer.commit_message(&msg.0, CommitMode::Async) {
                            log::info!("Failed to ack: {err}");
                            break;
                        }
                    }
                    Err(err) if !err.is_temporary() => {
                        log::info!("Dropping event with permanent error: {}", err);
                        if let Err(err) = self.consumer.commit_message(&msg.0, CommitMode::Async) {
                            log::info!("Failed to ack: {err}");
                            break;
                        }
                    }
                    Err(err) => {
                        log::info!("Failed to handle event: {}", err);
                        break;
                    }
                },
                Some(Err(err)) => {
                    log::warn!("Failed to receive from Kafka: {err}");
                    break;
                }
            };
        }
    }

    async fn handle(&self, event: Event, metrics: &mut Metrics) -> Result<(), TwinEventError> {
        self.process(TwinEvent::try_from(event)?, metrics).await?;

        Ok(())
    }

    async fn process(&self, event: TwinEvent, metrics: &mut Metrics) -> Result<(), TwinEventError> {
        TOTAL_EVENTS.inc();
        log::debug!("Processing twin event: {event:?}");

        if ! metrics.devices.contains(&event.device) {
            metrics.devices.push(event.device.clone());
            DEVICES_SEEN.inc();
        }

        // let collection = self.db.collection::<ThingState>(&event.application);
        //
        // let mut update = Document::new();
        // for (k, v) in event.features {
        //     let v: Bson = v.try_into()?;
        //     update.insert(format!("features.{k}.properties"), v);
        // }
        //
        // if update.is_empty() {
        //     return Ok(());
        // }
        //
        // update.insert(
        //     "lastUpdateTimestamp".to_string(),
        //     Bson::String("$currentDate".to_string()),
        // );



        log::debug!("Event: {:#?}", event);

        // collection
        //     .update_one(
        //         doc! {
        //             "device": event.device
        //         },
        //         update,
        //         Some(UpdateOptions::builder().upsert(true).build()),
        //     )
        //     .await?;

        Ok(())
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenv::dotenv().ok();
    env_logger::init();

    let config: ApplicationConfig = Config::builder()
        .add_source(Environment::default().separator("__"))
        .build()?
        .try_deserialize()?;

    let mut metrics = Metrics {devices: Vec::new() };

    log::info!("Configuration: {:#?}", config);

    let app = Processor::new(config).await?;

    // run

    app.run(&mut metrics).await;

    // done

    log::warn!("Kafka stream finished. Exiting...");

    Ok(())
}

#[derive(Clone, Debug)]
struct TwinEvent {
    pub application: String,
    pub device: String,
    pub features: Map<String, Value>,
}

#[derive(Clone, Debug, thiserror::Error)]
pub enum TwinEventError {
    #[error("Conversion error: {0}")]
    Conversion(String),
    #[error("Persistence error: {0}")]
    Persistence(#[from] mongodb::error::Error),
    #[error("Value error: {0}")]
    Value(#[from] bson::extjson::de::Error),
}

impl TwinEventError {
    pub fn is_temporary(&self) -> bool {
        match self {
            // Assume all MongoDB errors to be temporary. Might need some refinement.
            Self::Persistence(_) => true,
            _ => false,
        }
    }
}

impl TryFrom<Event> for TwinEvent {
    type Error = TwinEventError;

    fn try_from(event: Event) -> Result<Self, Self::Error> {
        let (application, device, mut payload) = match (
            event.extension("application").cloned(),
            event.extension("device").cloned(),
            payload(event),
        ) {
            (
                Some(ExtensionValue::String(application)),
                Some(ExtensionValue::String(device)),
                Some(payload),
            ) => {
                log::debug!("Payload: {:#?}", payload);
                (application, device, payload)
            }
            _ => {
                return Err(TwinEventError::Conversion("Unknown event".into()));
            }
        };

        let features: Map<String, Value> = serde_json::from_value(payload["features"].take())
            .map_err(|err| TwinEventError::Conversion(format!("Failed to convert: {err}")))?;

        Ok(TwinEvent {
            application,
            device,
            features,
        })
    }
}

impl TwinEvent {}

fn payload(mut event: Event) -> Option<Value> {
    if event.datacontenttype() != Some("application/json") {
        return None;
    }

    match event.take_data() {
        (_, _, Some(Data::Json(json))) => Some(json),
        (_, _, Some(Data::Binary(data))) => serde_json::from_slice(&data).ok(),
        (_, _, Some(Data::String(data))) => serde_json::from_str(&data).ok(),
        _ => None,
    }
}
