//! Defines the HSV5 "state machine"

use super::{ConnInitSettings, ConnectError, ConnectionReject};
use crate::{
    accesscontrol::StreamAcceptor,
    crypto::CryptoManager,
    packet::{
        HSV5Info, HandshakeControlInfo, HandshakeVSInfo, ServerRejectReason, SrtControlPacket,
        SrtHandshake, SrtShakeFlags,
    },
    ConnectionSettings, SrtVersion,
};
use std::{
    net::SocketAddr,
    time::{Duration, Instant},
};

pub enum GenHsv5Result {
    Accept(HandshakeVSInfo, ConnectionSettings),
    NotHandled(ConnectError),
    Reject(ConnectionReject),
}

pub fn gen_hsv5_response(
    settings: &mut ConnInitSettings,
    with_hsv5: &HandshakeControlInfo,
    from: SocketAddr,
    acceptor: &mut impl StreamAcceptor,
) -> GenHsv5Result {
    let incoming = match &with_hsv5.info {
        HandshakeVSInfo::V5(hs) => hs,
        _ => {
            return GenHsv5Result::Reject(ConnectionReject::Rejecting(
                ServerRejectReason::Version.into(), // TODO: this error is tehcnially reserved for access control handlers, as the ref impl supports hsv4+5, while we only support 5
            ));
        }
    };

    let mut accept_params = match acceptor.accept(incoming.sid.as_deref(), from) {
        Ok(ap) => ap,
        Err(rr) => return GenHsv5Result::Reject(ConnectionReject::Rejecting(rr)),
    };

    // apply parameters generated by acceptor
    if let Some(co) = accept_params.take_crypto_options() {
        settings.crypto = Some(co);
    }

    let hs = match incoming.ext_hs {
        Some(SrtControlPacket::HandshakeRequest(hs)) => hs,
        Some(_) => return GenHsv5Result::NotHandled(ConnectError::ExpectedHSReq),
        None => return GenHsv5Result::NotHandled(ConnectError::ExpectedExtFlags),
    };

    // crypto
    let cm = match (&settings.crypto, &incoming.ext_km) {
        // ok, both sizes have crypto
        (Some(co), Some(SrtControlPacket::KeyManagerRequest(km))) => {
            if co.size != incoming.crypto_size {
                unimplemented!("Key size mismatch");
            }

            Some(match CryptoManager::new_from_kmreq(co.clone(), km) {
                Ok(cm) => cm,
                Err(rr) => return GenHsv5Result::Reject(rr),
            })
        }
        // ok, neither have crypto
        (None, None) => None,
        // bad cases
        (Some(_), Some(_)) => unimplemented!("Expected kmreq"),
        (Some(_), None) | (None, Some(_)) => unimplemented!("Crypto mismatch"),
    };
    let outgoing_ext_km = if let Some(cm) = &cm {
        Some(cm.generate_km())
    } else {
        None
    };
    let sid = if let HandshakeVSInfo::V5(info) = &with_hsv5.info {
        info.sid.clone()
    } else {
        None
    };

    GenHsv5Result::Accept(
        HandshakeVSInfo::V5(HSV5Info {
            crypto_size: cm.as_ref().map(|c| c.key_length()).unwrap_or(0),
            ext_hs: Some(SrtControlPacket::HandshakeResponse(SrtHandshake {
                version: SrtVersion::CURRENT,
                flags: SrtShakeFlags::SUPPORTED,
                send_latency: settings.send_latency,
                recv_latency: settings.recv_latency,
            })),
            ext_km: outgoing_ext_km.map(SrtControlPacket::KeyManagerResponse),
            sid,
        }),
        ConnectionSettings {
            remote: from,
            remote_sockid: with_hsv5.socket_id,
            local_sockid: settings.local_sockid,
            socket_start_time: Instant::now(), // xxx?
            init_send_seq_num: settings.starting_send_seqnum,
            init_recv_seq_num: with_hsv5.init_seq_num,
            max_packet_size: 1500, // todo: parameters!
            max_flow_size: 8192,
            send_tsbpd_latency: Duration::max(settings.send_latency, hs.recv_latency),
            recv_tsbpd_latency: Duration::max(settings.recv_latency, hs.send_latency),
            crypto_manager: cm,
            stream_id: incoming.sid.clone(),
        },
    )
}

#[derive(Debug, Clone)] // TOOD: make not clone
pub struct StartedInitiator {
    cm: Option<CryptoManager>,
    settings: ConnInitSettings,
    streamid: Option<String>,
}

pub fn start_hsv5_initiation(
    settings: ConnInitSettings,
    streamid: Option<String>,
) -> (HandshakeVSInfo, StartedInitiator) {
    let self_crypto_size = settings.crypto.as_ref().map(|co| co.size).unwrap_or(0);

    // if peer_crypto_size != self_crypto_size {
    //     unimplemented!("Unimplemted crypto mismatch!");
    // }

    let (cm, ext_km) = if let Some(co) = &settings.crypto {
        let cm = CryptoManager::new_random(co.clone());
        let kmreq = SrtControlPacket::KeyManagerRequest(cm.generate_km());
        (Some(cm), Some(kmreq))
    } else {
        (None, None)
    };

    (
        HandshakeVSInfo::V5(HSV5Info {
            crypto_size: self_crypto_size,
            ext_hs: Some(SrtControlPacket::HandshakeRequest(SrtHandshake {
                version: SrtVersion::CURRENT,
                flags: SrtShakeFlags::SUPPORTED,
                send_latency: settings.send_latency,
                recv_latency: settings.recv_latency,
            })),
            ext_km,
            sid: streamid.clone(),
        }),
        StartedInitiator {
            cm,
            settings,
            streamid,
        },
    )
}

impl StartedInitiator {
    pub fn finish_hsv5_initiation(
        self,
        response: &HandshakeControlInfo,
        from: SocketAddr,
    ) -> Result<ConnectionSettings, ConnectError> {
        // TODO: factor this out with above...
        let incoming = match &response.info {
            HandshakeVSInfo::V5(hs) => hs,
            i => return Err(ConnectError::UnsupportedProtocolVersion(i.version())),
        };

        let hs = match incoming.ext_hs {
            Some(SrtControlPacket::HandshakeResponse(hs)) => hs,
            Some(_) => return Err(ConnectError::ExpectedHSResp),
            None => return Err(ConnectError::ExpectedExtFlags),
        };

        // todo: validate km!

        // validate response
        Ok(ConnectionSettings {
            remote: from,
            remote_sockid: response.socket_id,
            local_sockid: self.settings.local_sockid,
            socket_start_time: Instant::now(), // xxx?
            init_send_seq_num: self.settings.starting_send_seqnum,
            init_recv_seq_num: response.init_seq_num,
            max_packet_size: 1500, // todo: parameters!
            max_flow_size: 8192,
            send_tsbpd_latency: Duration::max(self.settings.send_latency, hs.recv_latency),
            recv_tsbpd_latency: Duration::max(self.settings.recv_latency, hs.send_latency),
            crypto_manager: self.cm,
            stream_id: self.streamid,
        })
    }
}