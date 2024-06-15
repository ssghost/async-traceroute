use std::cell::RefCell;
use std::cmp::min;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use async_stream::stream;
use futures_core::stream::Stream;
use tokio::select;
use tokio::task::JoinSet;

use crate::traceroute::probe::{ProbeError, ProbeResult};
use crate::traceroute::probe::generator::ProbeTaskGenerator;
use crate::traceroute::probe::sniffer::{IcmpProbeResponseSniffer, Sniffer};
use crate::traceroute::utils::dns;

pub mod utils;
pub mod terminal;
pub mod probe;
pub mod builder;
mod async_socket;

pub struct Traceroute {
    target_ip_address: IpAddr,
    max_ttl: u8,
    nqueries: u16,
    sim_queries: u16,
    max_wait_probe: Duration,
    is_active_dns_lookup: bool,
    current_ttl: Box<RefCell<u8>>,
    current_query: Box<RefCell<u16>>,
    probe_task_generator: Box<RefCell<Box<dyn ProbeTaskGenerator>>>,
    icmp_probe_response_sniffer: Arc<IcmpProbeResponseSniffer>,
}

impl Traceroute {
    pub fn new(
        ip_addr: IpAddr,
        max_ttl: u8,
        nqueries: u16,
        sim_queries: u16,
        max_wait_probe: Duration,
        is_active_dns_lookup: bool,
        probe_task_generator: Box<dyn ProbeTaskGenerator>,
        icmp_probe_response_sniffer: IcmpProbeResponseSniffer
    ) -> Self {
        Self {
            target_ip_address: ip_addr,
            max_ttl,
            nqueries,
            sim_queries: min(sim_queries, (max_ttl as u16) * nqueries),
            max_wait_probe,
            is_active_dns_lookup,
            current_ttl: Box::new(RefCell::new(1)),
            current_query: Box::new(RefCell::new(1)),
            probe_task_generator: Box::new(RefCell::new(probe_task_generator)),
            icmp_probe_response_sniffer: Arc::new(icmp_probe_response_sniffer),
        }
    }
    
    pub fn trace(self) -> impl Stream<Item = Result<ProbeResult, ProbeError>> {
        let mut probe_tasks = JoinSet::new();

        for _ in 0..self.sim_queries {
            let probe_task = self.generate_probe_task(&self.icmp_probe_response_sniffer);
            self.increment_ttl_query_counter();
            probe_tasks.spawn(probe_task);
        }

        let icmp_probe_response_sniffer = Arc::clone(&self.icmp_probe_response_sniffer);
        tokio::spawn(async move {
            icmp_probe_response_sniffer.sniff().await
        });

        let mut target_address_encountered_counter = 0;
        let mut stop_send_probes = false;
        stream! {
            loop {
                if *self.current_ttl.borrow() > self.max_ttl {
                    stop_send_probes = true;
                }

                select! {
                    Some(Ok(probe_result)) = probe_tasks.join_next() => {
                        match probe_result {
                            Ok(mut probe_result) => {
                                if self.is_active_dns_lookup {
                                    Self::reverse_dns_lookup(&mut probe_result).await;
                                }

                                if probe_result.from_address() == self.target_ip_address {
                                    target_address_encountered_counter += 1;
                                    if target_address_encountered_counter >= self.nqueries {
                                        stop_send_probes = true;
                                    }
                                }
                                
                                yield Ok(probe_result);

                                if !stop_send_probes {
                                    let probe_task = self.generate_probe_task(&self.icmp_probe_response_sniffer);
                                    self.increment_ttl_query_counter();
                                    probe_tasks.spawn(probe_task);
                                }
                            },
                            Err(_) => yield probe_result,
                        }
                    },
                    else => break
                }
            }
        }
    }

    fn generate_probe_task(
        &self, 
        icmp_probe_response_sniffer: &IcmpProbeResponseSniffer
    ) -> Pin<Box<impl Future<Output=Result<ProbeResult, ProbeError>>>> {
        
        let mut probe_task_generator = self.probe_task_generator.borrow_mut();
        match probe_task_generator.generate_probe_task(
            self.target_ip_address,
            &icmp_probe_response_sniffer
        ) {
            Ok((_, mut probe_task)) => {
                let current_ttl = *self.current_ttl.borrow();
                let timeout = self.max_wait_probe;
                let probe_task_future = Box::pin(async move {
                    probe_task.send_probe(current_ttl, timeout).await
                });
                probe_task_future
            }
            Err(_) => todo!()
        }
    }

    fn increment_ttl_query_counter(&self) {
        let mut current_query = self.current_query.borrow_mut();
        *current_query += 1;
        if *current_query > self.nqueries {
            *current_query = 1;
            let mut current_ttl = self.current_ttl.borrow_mut();
            *current_ttl += 1;
        }
    }
    
    async fn reverse_dns_lookup(probe_result: &mut ProbeResult) {
        let ip_addr = &IpAddr::V4(probe_result.from_address());
        if let Some(hostname) = dns::reverse_dns_lookup_first_hostname(ip_addr).await {
            probe_result.set_hostname(&hostname);
        }
    }
    
    pub fn get_nqueries(&self) -> u16 {
        self.nqueries
    }
    
    pub fn get_max_ttl(&self) -> u8 {
        self.max_ttl
    }
}