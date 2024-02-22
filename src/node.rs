use std::{
  collections::{BTreeMap, BTreeSet},
  sync::{Arc, Mutex},
  //pin::pin,
};

use futures::{pin_mut, FutureExt, StreamExt};
use async_channel::Receiver;
#[allow(unused_imports)]
use log::{debug, error, info, trace, warn};
use serde::{de::DeserializeOwned, Serialize};
use rustdds::{dds::CreateResult, *};

use crate::{
  action::*,
  context::Context,
  entities_info::{NodeEntitiesInfo, ParticipantEntitiesInfo},
  gid::Gid,
  log as ros_log,
  log::Log,
  names::*,
  parameters::*,
  pubsub::{Publisher, Subscription},
  service::{Client, Server, Service, ServiceMapping},
};

/// Configuration of [Node]
/// This is a builder-like struct.
#[must_use]
pub struct NodeOptions {
  #[allow(dead_code)]
  cli_args: Vec<String>,
  #[allow(dead_code)]
  use_global_arguments: bool, // process-wide command line args
  enable_rosout: bool, // use rosout topic for logging?
  enable_rosout_reading: bool,
  #[allow(dead_code)]
  start_parameter_services: bool,
  #[allow(dead_code)]
  parameter_overrides: Vec<Parameter>,
  #[allow(dead_code)]
  allow_undeclared_parameters: bool,
  #[allow(dead_code)]
  automatically_declare_parameters_from_overrides: bool,
  // The NodeOptions struct does not contain
  // node_name, context, or namespace, because
  // they ae always needed and have no reasonable default.
}

impl NodeOptions {
  /// Get a default NodeOptions
  pub fn new() -> NodeOptions {
    // These defaults are from rclpy reference
    // https://docs.ros2.org/latest/api/rclpy/api/node.html
    NodeOptions {
      cli_args: Vec::new(),
      use_global_arguments: true,
      enable_rosout: true,
      enable_rosout_reading: false,
      start_parameter_services: true,
      parameter_overrides: Vec::new(),
      allow_undeclared_parameters: false,
      automatically_declare_parameters_from_overrides: false,
    }
  }
  pub fn enable_rosout(self, enable_rosout: bool) -> NodeOptions {
    NodeOptions {
      enable_rosout,
      ..self
    }
  }

  pub fn read_rosout(self, enable_rosout_reading: bool) -> NodeOptions {
    NodeOptions {
      enable_rosout_reading,
      ..self
    }
  }
}

impl Default for NodeOptions {
  fn default() -> Self {
    Self::new()
  }
}
// ----------------------------------------------------------------------------------------------------
// ----------------------------------------------------------------------------------------------------

/// DDS or ROS 2 Discovery events.
#[derive(Clone, Debug)]
pub enum NodeEvent {
  DDS(DomainParticipantStatusEvent),
  ROS(ParticipantEntitiesInfo),
}

// ----------------------------------------------------------------------------------------------------
// ----------------------------------------------------------------------------------------------------
/// Spinner implements Node's background event loop.
///
/// At the moment there are only Discovery (DDS and ROS 2 Graph) event
/// processing, but this would be extended to handle Parameters and other
/// possible background tasks also.
pub struct Spinner {
  ros_context: Context,
  stop_spin_receiver: async_channel::Receiver<()>,

  readers_to_remote_writers: Arc<Mutex<BTreeMap<GUID, BTreeSet<GUID>>>>,
  writers_to_remote_readers: Arc<Mutex<BTreeMap<GUID, BTreeSet<GUID>>>>,
  // Keep track of ros_discovery_info
  external_nodes: Arc<Mutex<BTreeMap<Gid, Vec<NodeEntitiesInfo>>>>,

  status_event_senders: Arc<Mutex<Vec<async_channel::Sender<NodeEvent>>>>,
}

impl Spinner {
  pub async fn spin(self) -> CreateResult<()> {
    let dds_status_listener = self.ros_context.domain_participant().status_listener();
    let dds_status_stream = dds_status_listener.as_async_status_stream();
    pin_mut!(dds_status_stream);

    let ros_discovery_topic = self.ros_context.ros_discovery_topic();
    let ros_discovery_reader = self
      .ros_context
      .create_subscription::<ParticipantEntitiesInfo>(&ros_discovery_topic, None)?;
    let ros_discovery_stream = ros_discovery_reader.async_stream();
    pin_mut!(ros_discovery_stream);

    loop {
      futures::select! {
        _ = self.stop_spin_receiver.recv().fuse() => {
          break;
        }

        participant_info_update = ros_discovery_stream.select_next_some() => {
          //println!("{:?}", participant_info_update);
          match participant_info_update {
            Ok((part_update, _msg_info)) => {
              // insert to Node-local ros_discovery_info bookkeeping
              let mut info_map = self.external_nodes.lock().unwrap();
              info_map.insert( part_update.gid, part_update.node_entities_info_seq.clone());
              // also notify any status listeneners
              self.send_status_event( &NodeEvent::ROS(part_update) );
            }
            Err(_e) => {
              // warn!("ros_discovery_info error {e:?}");
            }
          }
        }

        dp_status_event = dds_status_stream.select_next_some() => {
          //println!("{:?}", dp_status_event );

          // update remote reader/writer databases
          match dp_status_event {
            DomainParticipantStatusEvent::RemoteReaderMatched { local_writer, remote_reader } => {
              self.writers_to_remote_readers.lock().unwrap()
                .entry(local_writer)
                .and_modify(|s| {s.insert(remote_reader);} )
                .or_insert(BTreeSet::from([remote_reader]));
            }
            DomainParticipantStatusEvent::RemoteWriterMatched { local_reader, remote_writer } => {
              self.readers_to_remote_writers.lock().unwrap()
                .entry(local_reader)
                .and_modify(|s| {s.insert(remote_writer);} )
                .or_insert(BTreeSet::from([remote_writer]));
            }
            DomainParticipantStatusEvent::ReaderLost {guid, ..} => {
              for ( _local, readers)
              in self.writers_to_remote_readers.lock().unwrap().iter_mut() {
                readers.remove(&guid);
              }
            }
            DomainParticipantStatusEvent::WriterLost {guid, ..} => {
              for ( _local, writers)
              in self.readers_to_remote_writers.lock().unwrap().iter_mut() {
                writers.remove(&guid);
              }
            }

            _ => {}
          }

          // also notify any status listeneners
          self.send_status_event( &NodeEvent::DDS(dp_status_event) );
        }
      }
    }
    info!("Spinner exiting .spin()");
    Ok(())
    //}
  } // fn

  fn send_status_event(&self, event: &NodeEvent) {
    let mut closed = Vec::new();
    let mut sender_array = self.status_event_senders.lock().unwrap();
    for (i, sender) in sender_array.iter().enumerate() {
      match sender.try_send(event.clone()) {
        Ok(()) => {}
        Err(async_channel::TrySendError::Closed(_)) => {
          closed.push(i) // mark for deletion
        }
        Err(_) => {}
      }
    }

    // remove senders that reported they were closed
    for c in closed.iter().rev() {
      sender_array.swap_remove(*c);
    }
  }
} // impl Spinner

// ----------------------------------------------------------------------------------------------------
// ----------------------------------------------------------------------------------------------------

/// Node in ROS2 network. Holds necessary readers and writers for rosout and
/// parameter events topics internally.
///
/// These are produced by a [`Context`].

// TODO: We should notify ROS discovery when readers or writers are removed, but
// now we do not do that.
pub struct Node {
  node_name: NodeName,
  options: NodeOptions,

  pub(crate) ros_context: Context,

  // sets of Readers and Writers belonging to ( = created via) this Node
  // These indicate what has been created locally.
  readers: BTreeSet<Gid>,
  writers: BTreeSet<Gid>,

  // Keep track of who is matched via DDS Discovery
  // Map keys are lists of local Subscriptions and Publishers.
  // Map values are lists of matched Publishers / Subscriptions.
  readers_to_remote_writers: Arc<Mutex<BTreeMap<GUID, BTreeSet<GUID>>>>,
  writers_to_remote_readers: Arc<Mutex<BTreeMap<GUID, BTreeSet<GUID>>>>,

  // Keep track of ros_discovery_info
  external_nodes: Arc<Mutex<BTreeMap<Gid, Vec<NodeEntitiesInfo>>>>,
  stop_spin_sender: Option<async_channel::Sender<()>>,

  // Channels to report discovery events
  status_event_senders: Arc<Mutex<Vec<async_channel::Sender<NodeEvent>>>>,

  // builtin writers and readers
  rosout_writer: Option<Publisher<Log>>,
  rosout_reader: Option<Subscription<Log>>,
  parameter_events_writer: Publisher<raw::ParameterEvent>,
}

impl Node {
  pub(crate) fn new(
    node_name: NodeName,
    options: NodeOptions,
    ros_context: Context,
  ) -> CreateResult<Node> {
    let paramtopic = ros_context.get_parameter_events_topic();
    let rosout_topic = ros_context.get_rosout_topic();

    let rosout_writer = if options.enable_rosout {
      Some(
        // topic already has QoS defined
        ros_context.create_publisher(&rosout_topic, None)?,
      )
    } else {
      None
    };
    let rosout_reader = if options.enable_rosout_reading {
      Some(ros_context.create_subscription(&rosout_topic, None)?)
    } else {
      None
    };

    let parameter_events_writer = ros_context.create_publisher(&paramtopic, None)?;

    Ok(Node {
      node_name,
      options,
      ros_context,
      readers: BTreeSet::new(),
      writers: BTreeSet::new(),
      readers_to_remote_writers: Arc::new(Mutex::new(BTreeMap::new())),
      writers_to_remote_readers: Arc::new(Mutex::new(BTreeMap::new())),
      external_nodes: Arc::new(Mutex::new(BTreeMap::new())),
      stop_spin_sender: None,
      status_event_senders: Arc::new(Mutex::new(Vec::new())),
      rosout_writer,
      rosout_reader,
      parameter_events_writer,
    })
  }

  /// Create a Spinner object to execute Node backround tasks.
  ///
  /// An async task should then be created to run the `.spin()` function of
  /// `Spinner`.
  ///
  /// E.g. `executor.spawn(node.spinner().spin())`
  ///
  /// The `.spin()` task runs until `Node` is dropped.
  pub fn spinner(&mut self) -> Spinner {
    if self.stop_spin_sender.is_some() {
      panic!("Attempted to crate a second spinner.");
    }
    let (stop_spin_sender, stop_spin_receiver) = async_channel::bounded(1);
    self.stop_spin_sender = Some(stop_spin_sender);

    Spinner {
      ros_context: self.ros_context.clone(),
      stop_spin_receiver,
      readers_to_remote_writers: Arc::clone(&self.readers_to_remote_writers),
      writers_to_remote_readers: Arc::clone(&self.writers_to_remote_readers),
      external_nodes: Arc::clone(&self.external_nodes),
      status_event_senders: Arc::clone(&self.status_event_senders),
    }
  }

  // Generates ROS2 node info from added readers and writers.
  fn generate_node_info(&self) -> NodeEntitiesInfo {
    let mut node_info = NodeEntitiesInfo::new(self.node_name.clone());

    node_info.add_writer(Gid::from(self.parameter_events_writer.guid()));
    if let Some(row) = &self.rosout_writer {
      node_info.add_writer(Gid::from(row.guid()));
    }

    for reader in &self.readers {
      node_info.add_reader(*reader);
    }

    for writer in &self.writers {
      node_info.add_writer(*writer);
    }

    node_info
  }

  fn add_reader(&mut self, reader: Gid) {
    self.readers.insert(reader);
    self.ros_context.update_node(self.generate_node_info());
  }

  fn add_writer(&mut self, writer: Gid) {
    self.writers.insert(writer);
    self.ros_context.update_node(self.generate_node_info());
  }

  pub fn base_name(&self) -> &str {
    self.node_name.base_name()
  }

  pub fn namespace(&self) -> &str {
    self.node_name.namespace()
  }

  pub fn fully_qualified_name(&self) -> String {
    self.node_name.fully_qualified_name()
  }

  pub fn options(&self) -> &NodeOptions {
    &self.options
  }

  pub fn domain_id(&self) -> u16 {
    self.ros_context.domain_id()
  }

  /// Get an async Receiver for discovery events.
  ///
  /// There must be an async task executing `spin` to get any data.
  pub fn status_receiver(&self) -> Receiver<NodeEvent> {
    let (status_event_sender, status_event_receiver) = async_channel::bounded(8);
    self
      .status_event_senders
      .lock()
      .unwrap()
      .push(status_event_sender);
    status_event_receiver
  }

  // reader waits for at least one writer to be present
  pub(crate) async fn wait_for_writer(&self, reader: GUID) {
    // TODO: This may contain some synchrnoization hazard
    let status_receiver = self.status_receiver();
    pin_mut!(status_receiver);

    let already_present = self
      .readers_to_remote_writers
      .lock()
      .unwrap()
      .get(&reader)
      .map(|writers| !writers.is_empty()) // there is someone matched
      .unwrap_or(false); // we do not even know the reader

    if !already_present {
      loop {
        // waiting loop
        if let NodeEvent::DDS(DomainParticipantStatusEvent::RemoteWriterMatched {
          local_reader,
          ..
        }) = status_receiver.select_next_some().await
        {
          if local_reader == reader {
            break; // we got a match
          }
        }
      }
    }
  }

  pub(crate) async fn wait_for_reader(&self, writer: GUID) {
    let status_receiver = self.status_receiver();
    pin_mut!(status_receiver);

    let already_present = self
      .writers_to_remote_readers
      .lock()
      .unwrap()
      .get(&writer)
      .map(|readers| !readers.is_empty()) // there is someone matched
      .unwrap_or(false); // we do not even know who is asking

    if !already_present {
      loop {
        if let NodeEvent::DDS(DomainParticipantStatusEvent::RemoteReaderMatched {
          local_writer,
          ..
        }) = status_receiver.select_next_some().await
        {
          if local_writer == writer {
            break; // we got a match
          }
        }
      }
    }
  }

  pub(crate) fn get_publisher_count(&self, subscription_guid: GUID) -> usize {
    self
      .readers_to_remote_writers
      .lock()
      .unwrap()
      .get(&subscription_guid)
      .map(BTreeSet::len)
      .unwrap_or_else(|| {
        error!("get_publisher_count: Subscriber {subscription_guid:?} not known to node.");
        0
      })
  }

  pub(crate) fn get_subscription_count(&self, publisher_guid: GUID) -> usize {
    self
      .writers_to_remote_readers
      .lock()
      .unwrap()
      .get(&publisher_guid)
      .map(BTreeSet::len)
      .unwrap_or_else(|| {
        error!("get_subscription_count: Publisher {publisher_guid:?} not known to node.");
        0
      })
  }

  /// Borrow the Subscription to our ROSOut Reader.
  ///
  /// Availability depends on Node configuration.
  pub fn rosout_subscription(&self) -> Option<&Subscription<Log>> {
    self.rosout_reader.as_ref()
  }

  #[allow(clippy::too_many_arguments)]
  pub fn rosout_raw(
    &self,
    timestamp: Timestamp,
    level: crate::ros2::LogLevel,
    log_name: &str,
    log_msg: &str,
    source_file: &str,
    source_function: &str,
    source_line: u32,
  ) {
    match &self.rosout_writer {
      None => debug!("Rosout not enabled. msg: {log_msg}"),
      Some(writer) => {
        writer
          .publish(ros_log::Log {
            timestamp,
            level: level as u8,
            name: log_name.to_string(),
            msg: log_msg.to_string(),
            file: source_file.to_string(),
            function: source_function.to_string(),
            line: source_line,
          })
          .unwrap_or_else(|e| debug!("Rosout publish failed: {e:?}"));
      }
    }
  }

  /// Creates ROS2 topic and handles necessary conversions from DDS to ROS2
  ///
  /// # Arguments
  ///
  /// * `domain_participant` -
  ///   [DomainParticipant](../dds/struct.DomainParticipant.html)
  /// * `name` - Name of the topic
  /// * `type_name` - What type the topic holds in string form
  /// * `qos` - Quality of Service parameters for the topic (not restricted only
  ///   to ROS2)
  ///
  ///  
  ///   [summary of all rules for topic and service names in ROS 2](https://design.ros2.org/articles/topic_and_service_names.html)
  ///   (as of Dec 2020)
  ///
  /// * must not be empty
  /// * may contain alphanumeric characters ([0-9|a-z|A-Z]), underscores (_), or
  ///   forward slashes (/)
  /// * may use balanced curly braces ({}) for substitutions
  /// * may start with a tilde (~), the private namespace substitution character
  /// * must not start with a numeric character ([0-9])
  /// * must not end with a forward slash (/)
  /// * must not contain any number of repeated forward slashes (/)
  /// * must not contain any number of repeated underscores (_)
  /// * must separate a tilde (~) from the rest of the name with a forward slash
  ///   (/), i.e. ~/foo not ~foo
  /// * must have balanced curly braces ({}) when used, i.e. {sub}/foo but not
  ///   {sub/foo nor /foo}
  pub fn create_topic(
    &self,
    topic_name: &Name,
    type_name: MessageTypeName,
    qos: &QosPolicies,
  ) -> CreateResult<Topic> {
    //let dds_name = Self::check_name_and_add_prefix("rt/", topic_name)?;
    let dds_name = topic_name.to_dds_name("rt", &self.node_name, "");
    info!("Creating topic, DDS name: {}", dds_name);
    let topic = self.ros_context.domain_participant().create_topic(
      dds_name,
      type_name.dds_msg_type(),
      qos,
      TopicKind::NoKey,
    )?;
    // ROS2 does not use WithKey topics, so always NoKey
    info!("Created topic");
    Ok(topic)
  }

  /// Creates ROS2 Subscriber
  ///
  /// # Arguments
  ///
  /// * `topic` - Reference to topic created with `create_ros_topic`.
  /// * `qos` - Should take [QOS](../dds/qos/struct.QosPolicies.html) and use if
  ///   it's compatible with topics QOS. `None` indicates the use of Topics QOS.
  pub fn create_subscription<D: DeserializeOwned + 'static>(
    &mut self,
    topic: &Topic,
    qos: Option<QosPolicies>,
  ) -> CreateResult<Subscription<D>> {
    let sub = self.ros_context.create_subscription(topic, qos)?;
    self.add_reader(sub.guid().into());
    Ok(sub)
  }

  /// Creates ROS2 Publisher
  ///
  /// # Arguments
  ///
  /// * `topic` - Reference to topic created with `create_ros_topic`.
  /// * `qos` - Should take [QOS](../dds/qos/struct.QosPolicies.html) and use it
  ///   if it's compatible with topics QOS. `None` indicates the use of Topics
  ///   QOS.
  pub fn create_publisher<D: Serialize>(
    &mut self,
    topic: &Topic,
    qos: Option<QosPolicies>,
  ) -> CreateResult<Publisher<D>> {
    let p = self.ros_context.create_publisher(topic, qos)?;
    self.add_writer(p.guid().into());
    Ok(p)
  }

  pub(crate) fn create_simpledatareader<D, DA>(
    &mut self,
    topic: &Topic,
    qos: Option<QosPolicies>,
  ) -> CreateResult<no_key::SimpleDataReader<D, DA>>
  where
    D: 'static,
    DA: rustdds::no_key::DeserializerAdapter<D> + 'static,
  {
    self.ros_context.create_simpledatareader(topic, qos)
  }

  pub(crate) fn create_datawriter<D, SA>(
    &mut self,
    topic: &Topic,
    qos: Option<QosPolicies>,
  ) -> CreateResult<no_key::DataWriter<D, SA>>
  where
    SA: rustdds::no_key::SerializerAdapter<D>,
  {
    self.ros_context.create_datawriter(topic, qos)
  }

  /// Creates ROS2 Service Client
  ///
  /// # Arguments
  ///
  /// * `service_mapping` - ServiceMapping to be used
  /// * `service_name` -
  /// * `qos`-
  pub fn create_client<S>(
    &mut self,
    service_mapping: ServiceMapping,
    service_name: &Name,
    service_type_name: &ServiceTypeName,
    request_qos: QosPolicies,
    response_qos: QosPolicies,
  ) -> CreateResult<Client<S>>
  where
    S: Service + 'static,
    S::Request: Clone,
  {
    // Add rq/ and rr/ prefixes as documented in
    // https://design.ros2.org/articles/topic_and_service_names.html
    // Where are the suffixes documented?
    // And why "Reply" and not "Response" ?

    let rq_topic = self.ros_context.domain_participant().create_topic(
      service_name.to_dds_name("rq", &self.node_name, "Request"),
      //rq_name,
      service_type_name.dds_request_type(),
      &request_qos,
      TopicKind::NoKey,
    )?;
    let rs_topic = self.ros_context.domain_participant().create_topic(
      service_name.to_dds_name("rr", &self.node_name, "Reply"),
      //rs_name,
      service_type_name.dds_response_type(),
      &response_qos,
      TopicKind::NoKey,
    )?;

    let c = Client::<S>::new(
      service_mapping,
      self,
      &rq_topic,
      &rs_topic,
      Some(request_qos),
      Some(response_qos),
    )?;

    Ok(c)
  }

  /// Creates ROS2 Service Server
  ///
  /// # Arguments
  ///
  /// * `service_mapping` - ServiceMapping to be used. See
  ///   [`Self.create_client`].
  /// * `service_name` -
  /// * `qos`-
  pub fn create_server<S>(
    &mut self,
    service_mapping: ServiceMapping,
    service_name: &Name,
    service_type_name: &ServiceTypeName,
    request_qos: QosPolicies,
    response_qos: QosPolicies,
  ) -> CreateResult<Server<S>>
  where
    S: Service + 'static,
    S::Request: Clone,
  {
    // let rq_name = Self::check_name_and_add_prefix("rq/",
    // &(service_name.to_owned() + "Request"))?; let rs_name =
    // Self::check_name_and_add_prefix("rr/", &(service_name.to_owned() +
    // "Reply"))?;

    let rq_topic = self.ros_context.domain_participant().create_topic(
      //rq_name,
      service_name.to_dds_name("rq", &self.node_name, "Request"),
      service_type_name.dds_request_type(),
      &request_qos,
      TopicKind::NoKey,
    )?;
    let rs_topic = self.ros_context.domain_participant().create_topic(
      service_name.to_dds_name("rr", &self.node_name, "Reply"),
      service_type_name.dds_response_type(),
      &response_qos,
      TopicKind::NoKey,
    )?;

    let s = Server::<S>::new(
      service_mapping,
      self,
      &rq_topic,
      &rs_topic,
      Some(request_qos),
      Some(response_qos),
    )?;

    Ok(s)
  }

  pub fn create_action_client<A>(
    &mut self,
    service_mapping: ServiceMapping,
    action_name: &Name,
    action_type_name: &ActionTypeName,
    action_qos: ActionClientQosPolicies,
  ) -> CreateResult<ActionClient<A>>
  where
    A: ActionTypes + 'static,
  {
    // action name is e.g. "/turtle1/rotate_absolute"
    // action type name is e.g. "turtlesim/action/RotateAbsolute"
    let services_base_name = action_name.push("_action");

    //let goal_service_name = action_name.to_owned() + "/_action/send_goal";
    let goal_service_type = action_type_name.dds_action_service("_SendGoal");
    let my_goal_client = self.create_client(
      service_mapping,
      //&goal_service_name,
      &services_base_name.push("send_goal"),
      &goal_service_type,
      action_qos.goal_service.clone(),
      action_qos.goal_service,
    )?;

    //let cancel_service_name = action_name.to_owned() + "/_action/cancel_goal";
    let cancel_goal_type = ServiceTypeName::new("action_msgs", "CancelGoal");
    let my_cancel_client = self.create_client(
      service_mapping,
      //&cancel_service_name,
      &services_base_name.push("cancel_goal"),
      &cancel_goal_type,
      action_qos.cancel_service.clone(),
      action_qos.cancel_service,
    )?;

    //let result_service_name = action_name.to_owned() + "/_action/get_result";
    let result_service_type = action_type_name.dds_action_service("_GetResult");
    let my_result_client = self.create_client(
      service_mapping,
      //&result_service_name,
      &services_base_name.push("get_result"),
      &result_service_type,
      action_qos.result_service.clone(),
      action_qos.result_service,
    )?;

    let action_topic_namespace = action_name.push("_action");

    let feedback_topic_type = action_type_name.dds_action_topic("_FeedbackMessage");
    let feedback_topic = self.create_topic(
      &action_topic_namespace.push("feedback"),
      feedback_topic_type,
      &action_qos.feedback_subscription,
    )?;
    let my_feedback_subscription =
      self.create_subscription(&feedback_topic, Some(action_qos.feedback_subscription))?;

    //let status_topic_type = ;
    let status_topic = self.create_topic(
      &action_topic_namespace.push("status"),
      MessageTypeName::new("action_msgs", "GoalStatusArray"),
      &action_qos.status_subscription,
    )?;
    let my_status_subscription =
      self.create_subscription(&status_topic, Some(action_qos.status_subscription))?;

    Ok(ActionClient {
      my_goal_client,
      my_cancel_client,
      my_result_client,
      my_feedback_subscription,
      my_status_subscription,
      my_action_name: action_name.clone(),
    })
  }

  pub fn create_action_server<A>(
    &mut self,
    service_mapping: ServiceMapping,
    action_name: &Name,
    action_type_name: &ActionTypeName,
    action_qos: ActionServerQosPolicies,
  ) -> CreateResult<ActionServer<A>>
  where
    A: ActionTypes + 'static,
  {
    let services_base_name = action_name.push("_action");

    //let goal_service_name = action_name.to_owned() + "/_action/send_goal";
    let goal_service_type = action_type_name.dds_action_service("_SendGoal");
    let my_goal_server = self.create_server(
      service_mapping,
      //&goal_service_name,
      &services_base_name.push("send_goal"),
      &goal_service_type,
      action_qos.goal_service.clone(),
      action_qos.goal_service,
    )?;

    //let cancel_service_name = action_name.to_owned() + "/_action/cancel_goal";
    let cancel_service_type = ServiceTypeName::new("action_msgs", "CancelGoal");
    let my_cancel_server = self.create_server(
      service_mapping,
      //&cancel_service_name,
      &services_base_name.push("cancel_goal"),
      &cancel_service_type,
      action_qos.cancel_service.clone(),
      action_qos.cancel_service,
    )?;

    //let result_service_name = action_name.to_owned() + "/_action/get_result";
    let result_service_type = action_type_name.dds_action_service("_GetResult");
    let my_result_server = self.create_server(
      service_mapping,
      //&result_service_name,
      &services_base_name.push("get_result"),
      &result_service_type,
      action_qos.result_service.clone(),
      action_qos.result_service,
    )?;

    let action_topic_namespace = action_name.push("_action");

    let feedback_topic_type = action_type_name.dds_action_topic("_FeedbackMessage");
    let feedback_topic = self.create_topic(
      &action_topic_namespace.push("feedback"),
      feedback_topic_type,
      &action_qos.feedback_publisher,
    )?;
    let my_feedback_publisher =
      self.create_publisher(&feedback_topic, Some(action_qos.feedback_publisher))?;

    let status_topic_type = MessageTypeName::new("action_msgs", "GoalStatusArray");
    let status_topic = self.create_topic(
      &action_topic_namespace.push("status"),
      status_topic_type,
      &action_qos.status_publisher,
    )?;
    let my_status_publisher =
      self.create_publisher(&status_topic, Some(action_qos.status_publisher))?;

    Ok(ActionServer {
      my_goal_server,
      my_cancel_server,
      my_result_server,
      my_feedback_publisher,
      my_status_publisher,
      my_action_name: action_name.clone(),
    })
  }
} // impl Node

impl Drop for Node {
  fn drop(&mut self) {
    if let Some(ref stop_spin_sender) = self.stop_spin_sender {
      stop_spin_sender
        .try_send(())
        .unwrap_or_else(|e| error!("Cannot notify spin task to stop: {e:?}"));
    }

    self
      .ros_context
      .remove_node(self.fully_qualified_name().as_str());
  }
}

/// Macro for writing to [rosout](https://wiki.ros.org/rosout) topic.
///
/// # Example
///
/// ```
/// # use ros2_client::*;
/// #
/// # let context = Context::new().unwrap();
/// # let mut node = context
/// #     .new_node(
/// #       NodeName::new("", "some_node").unwrap(),
/// #       NodeOptions::new().enable_rosout(true),
/// #     )
/// #     .unwrap();
/// let kind = "silly";
///
/// rosout!(node, ros2::LogLevel::Info, "A {} event was seen.", kind);
/// ```
#[macro_export]
macro_rules! rosout {
    // rosout!(node, Level::Info, "a {} event", event.kind);

    ($node:expr, $lvl:expr, $($arg:tt)+) => (
        $node.rosout_raw(
            $crate::ros2::Timestamp::now(),
            $lvl,
            $node.base_name(),
            &std::format!($($arg)+), // msg
            std::file!(),
            "<unknown_func>", // is there a macro to get current function name? (Which may be undefined)
            std::line!(),
        );
    );
}
