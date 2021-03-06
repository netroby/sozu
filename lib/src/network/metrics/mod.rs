use std::str;
use std::thread;
use std::sync::Mutex;
use std::cell::RefCell;
use std::time::{Duration,Instant};
use std::iter::repeat;
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::fmt::Arguments;
use std::net::SocketAddr;
use mio::net::UdpSocket;
use std::io::{self,BufWriter,Write,Error,ErrorKind};
use nom::HexDisplay;
use hdrhistogram::Histogram;
use sozu_command::buffer::Buffer;
use sozu_command::messages::{FilteredData,MetricsData,Percentiles,BackendMetricsData,FilteredTimeSerie};

mod network_drain;
mod local_drain;

use self::network_drain::NetworkDrain;
use self::local_drain::LocalDrain;

thread_local! {
  pub static METRICS: RefCell<Aggregator> = RefCell::new(Aggregator::new(String::from("sozu")));
}

#[derive(Debug,Clone,PartialEq)]
pub enum MetricData {
  Gauge(usize),
  Count(i64),
  Time(usize),
}

impl MetricData {
  fn is_time(&self) -> bool {
    match self {
      &MetricData::Time(_) => true,
      _ => false,
    }
  }

  fn update(&mut self, key: &'static str, m: MetricData) {
    match (self, m) {
      (&mut MetricData::Gauge(ref mut v1), MetricData::Gauge(v2)) => {
        *v1 = v2;
      },
      (&mut MetricData::Count(ref mut v1), MetricData::Count(v2)) => {
        *v1 += v2;
      },
      (s,m) => panic!("tried to update metric {} of value {:?} with an incompatible metric: {:?}", key, s, m)
    }
  }
}

#[derive(Debug,Clone)]
pub struct StoredMetricData {
  last_sent: Instant,
  data:      MetricData,
}

pub trait Subscriber {
  fn receive_metric(&mut self, label: &'static str, app_id: Option<&str>, backend_id: Option<&str>, metric: MetricData);
}

pub struct Aggregator {
  prefix:  String,
  network: Option<NetworkDrain>,
  local:   LocalDrain,
}

impl Aggregator {
  pub fn new(prefix: String) -> Aggregator {
    Aggregator {
      prefix: prefix.clone(),
      network: None,
      local: LocalDrain::new(prefix),
    }
  }

  pub fn set_up_remote(&mut self, socket: UdpSocket, addr: SocketAddr) {
    self.network = Some(NetworkDrain::new(self.prefix.clone(), socket, addr));
  }

  pub fn set_up_origin(&mut self, origin: String) {
    self.network.as_mut().map(|n| n.origin = origin);
  }

  pub fn set_up_tagged_metrics(&mut self, tagged: bool) {
    self.network.as_mut().map(|n| n.use_tagged_metrics = tagged);
  }

  pub fn socket(&self) -> Option<&UdpSocket> {
    self.network.as_ref().map(|n| &n.remote.get_ref().socket)
  }

  pub fn count_add(&mut self, key: &'static str, count_value: i64) {
    self.receive_metric(key, None, None, MetricData::Count(count_value));
  }

  pub fn set_gauge(&mut self, key: &'static str, gauge_value: usize) {
    self.receive_metric(key, None, None, MetricData::Gauge(gauge_value));
  }

  pub fn writable(&mut self) {
    if let Some(ref mut net) = self.network.as_mut() {
      net.writable();
    }
  }

  pub fn send_data(&mut self) {
    if let Some(ref mut net) = self.network.as_mut() {
      net.send_data();
    }
  }

  pub fn dump_metrics_data(&mut self) -> MetricsData {
    self.local.dump_metrics_data()
  }

  pub fn dump_process_data(&mut self) -> BTreeMap<String, FilteredData> {
    self.local.dump_process_data()
  }
}

impl Subscriber for Aggregator {
  fn receive_metric(&mut self, label: &'static str, app_id: Option<&str>, backend_id: Option<&str>, metric: MetricData) {
    if let Some(ref mut net) = self.network.as_mut() {
      net.receive_metric(label, app_id, backend_id, metric.clone());
    }
    self.local.receive_metric(label, app_id, backend_id, metric.clone());
  }
}

#[derive(Debug,Clone,PartialEq)]
pub struct MetricLine {
  label:      &'static str,
  app_id:     Option<String>,
  backend_id: Option<String>,
  /// in milliseconds
  duration:   usize,
}

pub struct MetricSocket {
  pub addr:   SocketAddr,
  pub socket: UdpSocket,
}


impl Write for MetricSocket {
  fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
    self.socket.send_to(buf, &self.addr)
  }

  fn flush(&mut self) -> io::Result<()> {
    Ok(())
  }
}

pub fn udp_bind() -> UdpSocket {
  UdpSocket::bind(&("0.0.0.0:0".parse().unwrap())).expect("could not parse address")
}

#[macro_export]
macro_rules! metrics_set_up (
  ($host:expr, $port: expr, $origin: expr, $use_tagged_metrics: expr) => ({
    use std::net::ToSocketAddrs;
    let metrics_socket = $crate::network::metrics::udp_bind();

    debug!("setting up metrics: local address = {:#?}", metrics_socket.local_addr());
    let metrics_host   = ($host, $port).to_socket_addrs().expect("could not parse address").next().expect("could not get first address");
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_up_remote(metrics_socket, metrics_host);
      (*metrics.borrow_mut()).set_up_origin($origin);
      (*metrics.borrow_mut()).set_up_tagged_metrics($use_tagged_metrics);
    });
  })
);

#[macro_export]
macro_rules! count (
  ($key:expr, $value: expr) => {
    let v = $value;
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).count_add($key, v);
    });
  }
);

#[macro_export]
macro_rules! incr (
  ($key:expr) => (count!($key, 1);)
);

#[macro_export]
macro_rules! decr (
  ($key:expr) => (count!($key, -1);)
);

#[macro_export]
macro_rules! gauge (
  ($key:expr, $value: expr) => {
    let v = $value;
    $crate::network::metrics::METRICS.with(|metrics| {
      //(*metrics.borrow_mut()).write(format_args!("{}.{}:{}|g\n", *$crate::logging::TAG, $key, v));
      (*metrics.borrow_mut()).set_gauge($key, v);
    });
  }
);

#[macro_export]
macro_rules! time_begin (
  ($key:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_time_begin($key);
    });
  }
);

#[macro_export]
macro_rules! time_end (
  ($key:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      (*metrics.borrow_mut()).set_time_end($key);
    });
  }
);

#[macro_export]
macro_rules! record_request_time (
  ($app_id:expr, $value: expr) => {
    use std::time::Instant;
    use $crate::network::metrics::{MetricData,Subscriber};
    let v = $value;
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $app_id;

      m.receive_metric("request_time", Some(key), None, MetricData::Time($value as usize));
      /*
      if m.app_data.contains_key(key) {
        let metrics = m.app_data.get_mut(key).unwrap();
        metrics.response_time.record($value as u64);
      } else {
        if let Ok(mut hist) = ::hdrhistogram::Histogram::new(3) {
          hist.record($value as u64);
          let metrics = $crate::network::metrics::AppMetrics {
            response_time: hist,
            last_sent: ::std::time::Instant::now(),
          };
          m.app_data.insert(key.to_string(), metrics);
        }
      }*/
    });
  }
);

#[macro_export]
macro_rules! record_backend_metrics (
  ($app_id:expr, $backend_id:expr, $response_time: expr, $bin: expr, $bout: expr) => {
    use std::time::Instant;
    use $crate::network::metrics::{MetricData,Subscriber};
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      let app_id: &str = $app_id.as_str();
      let backend_id: &str = $backend_id;

      m.receive_metric("bin", Some(app_id), Some(backend_id), MetricData::Count($bin as i64));
      m.receive_metric("bout", Some(app_id), Some(backend_id), MetricData::Count($bout as i64));
      m.receive_metric("response_time", Some(app_id), Some(backend_id), MetricData::Time($response_time as usize));
      /*
      if m.backend_data.contains_key(key) {
        let bm = m.backend_data.get_mut(key).unwrap();
        bm.response_time.record($response_time as u64);
        bm.bin += $bin;
        bm.bout += $bout;
      } else {
        if let Ok(hist) = ::hdrhistogram::Histogram::new(3) {
          let mut bm = $crate::network::metrics::BackendMetrics::new($app_id.clone(), hist);
          bm.response_time.record($response_time as u64);
          bm.bin += $bin;
          bm.bout += $bout;
          m.backend_data.insert(key.to_string(), bm);
        }
      }*/
    });
  }
);

#[macro_export]
macro_rules! remove_app_metrics (
  ($app_id:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      //FIXME!!!
      /*
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $app_id;
      m.app_data.remove(key);
      */
    });
  }
);

#[macro_export]
macro_rules! remove_backend_metrics (
  ($backend_id:expr) => {
    $crate::network::metrics::METRICS.with(|metrics| {
      //FIXME!!!
      /*
      let ref mut m = *metrics.borrow_mut();
      let key: &str = $backend_id;
      m.backend_data.remove(key);
      */
    });
  }
);

///Client-side request errors caused by:
///
/// * Client terminates before sending request
/// * Read error from client
/// * Client timeout
/// * Client terminated connection
#[macro_export]
macro_rules! incr_ereq (
  () => (incr!("ereq");)
);

#[macro_export]
macro_rules! incr_client_cmd (
  () => (incr!("client_cmd");)
);

#[macro_export]
macro_rules! incr_resp_client_cmd (
  () => (incr!("incr_resp_client_cmd");)
);

/// count another accepted request
#[macro_export]
macro_rules! incr_req (
  () => {
    use $crate::network::metrics::{MetricData,Subscriber};
    $crate::network::metrics::METRICS.with(|metrics| {
      let ref mut m = *metrics.borrow_mut();
      m.receive_metric("request_counter", None, None, MetricData::Count(1));
    });
  }
);
