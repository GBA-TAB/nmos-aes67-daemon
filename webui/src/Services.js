//
//  Services.js
//
//  Copyright (c) 2019 2020 Andrea Bondavalli. All rights reserved.
//
//  This program is free software: you can redistribute it and/or modify
//  it under the terms of the GNU General Public License as published by
//  the Free Software Foundation, either version 3 of the License, or
//  any later version.
//
//  This program is distributed in the hope that it will be useful,
//  but WITHOUT ANY WARRANTY; without even the implied warranty of
//  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
//  GNU General Public License for more details.
//
//  You should have received a copy of the GNU General Public License
//  along with this program.  If not, see <http://www.gnu.org/licenses/>.
//
//

import { toast } from 'react-toastify'; 

const API = '/api';
const version = '/version';
const config = '/config';
const streams = '/streams';
const sources = '/sources';
const sinks = '/sinks';
const ptpConfig = '/ptp/config';
const ptpStatus = '/ptp/status';
const source = '/source';
const sdp = '/sdp';
const sink = '/sink';
const status = '/status';
const browseSources = '/browse/sources/all';
const channelMapping = '/x-nmos/channelmapping/v1.0';
const nodeApi = '/x-nmos/node/v1.3';

const defaultParams = {
  credentials: 'same-origin',
  redirect: 'error',
  headers: new Headers({
    'X-USER-ID': 'test'
  })
};

// Cross-origin (different port than the page), so no custom headers here:
// a non-simple header would force a CORS preflight, and the NMOS server's
// Access-Control-Allow-Headers doesn't list X-USER-ID (which nothing
// server-side reads anyway).
const rawParams = {
  redirect: 'error'
};

export default class RestAPI {

  static getBaseUrl() {
    return location.protocol + '//' + location.host;
  }

  static doFetch(url, params = {}) {
    if (params.method === undefined) {
      params.method = 'GET';
    }

    return fetch(this.getBaseUrl() + API + url, Object.assign({}, defaultParams, params))
      .then(
        response => {
          if (response.ok) {
            return response;
          }
          console.log(this.getBaseUrl() + API + url + ' HTTP ' + response.status);
          return Promise.reject(Error('HTTP ' + response.status));
        }
      ).catch(
        err => {
          console.log(this.getBaseUrl() + API + url + ' failed: ' + err.message);
          return Promise.reject(Error(err.message));
        }
      );
  }

  // The NMOS node API (IS-08/IS-12) is served on its own port
  // (config.nmos_node_port), separate from the webui/API port this page was
  // loaded from - so it can't be reached at same-origin like /api/* can.
  static getNmosBaseUrl() {
    if (this._nmosBaseUrl) {
      return Promise.resolve(this._nmosBaseUrl);
    }
    return fetch(this.getBaseUrl() + API + config, defaultParams)
      .then(response => response.json())
      .then(cfg => {
        this._nmosBaseUrl = location.protocol + '//' + location.hostname + ':' + cfg.nmos_node_port;
        return this._nmosBaseUrl;
      });
  }

  // Like doFetch, but against the daemon's real NMOS API paths directly
  // (e.g. IS-08 Channel Mapping) instead of the /api proxy layer.
  static doFetchRaw(url, params = {}) {
    if (params.method === undefined) {
      params.method = 'GET';
    }

    return this.getNmosBaseUrl().then(base =>
      fetch(base + url, Object.assign({}, rawParams, params))
        .then(
          response => {
            if (response.ok) {
              return response;
            }
            // Surface the daemon's own {"error": "..."} reason instead of a
            // bare status code - IS-08 4xx responses always carry one.
            return response.json().then(
              body => Promise.reject(Error(body.error || ('HTTP ' + response.status))),
              () => Promise.reject(Error('HTTP ' + response.status))
            );
          }
        ).catch(
          err => {
            console.log(base + url + ' failed: ' + err.message);
            return Promise.reject(Error(err.message));
          }
        )
    );
  }

  static getNmosReceivers() {
    return this.doFetchRaw(nodeApi + '/receivers/').catch(err => {
      toast.error('NMOS receivers get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getNmosSenders() {
    return this.doFetchRaw(nodeApi + '/senders/').catch(err => {
      toast.error('NMOS senders get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getNmosSources() {
    return this.doFetchRaw(nodeApi + '/sources/').catch(err => {
      toast.error('NMOS sources get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getVersion() {
    return this.doFetch(version).catch(err => {
      toast.error('Config get failed: ' + err.message);
      return Promise.reject(Error(err.message));
    });
  }

  static getConfig() {
    return this.doFetch(config).catch(err => {
      toast.error('Config get failed: ' + err.message);
      return Promise.reject(Error(err.message));
    });
  }

  static setConfig(log_severity, syslog_proto, syslog_server, rtp_mcast_base, rtp_mcast_base_sec, rtp_port, rtp_port_sec, rtsp_port, playout_delay, tic_frame_size_at_1fs, sample_rate, max_tic_frame_size, sap_mcast_addr, sap_interval, mdns_enabled, custom_node_id, auto_sinks_update, streamer_enabled, streamer_channels, streamer_files_num, streamer_file_duration, streamer_player_buffer_files_num, nmos_enabled, nmos_registry_autodiscovery, nmos_registry_address, nmos_registry_port, nmos_registry_query_port, nmos_control_interface, nmos_node_port, nmos_mdns_enabled) {
    return this.doFetch(config, {
      body: JSON.stringify({
        log_severity: parseInt(log_severity, 10),
        syslog_proto: syslog_proto,
        syslog_server: syslog_server,
        rtp_mcast_base: rtp_mcast_base,
        rtp_mcast_base_sec: rtp_mcast_base_sec,
        rtp_port: parseInt(rtp_port, 10),
        rtp_port_sec: parseInt(rtp_port_sec, 10),
        rtsp_port: parseInt(rtsp_port, 10),
        playout_delay: parseInt(playout_delay, 10),
        tic_frame_size_at_1fs: parseInt(tic_frame_size_at_1fs, 10),
        sample_rate: parseInt(sample_rate, 10),
        max_tic_frame_size: parseInt(max_tic_frame_size, 10),
        sap_mcast_addr: sap_mcast_addr,
        sap_interval: parseInt(sap_interval, 10),
        custom_node_id: custom_node_id,
        mdns_enabled: mdns_enabled,
        auto_sinks_update: auto_sinks_update,
        streamer_enabled: streamer_enabled,
        streamer_channels: parseInt(streamer_channels, 10),
        streamer_files_num: parseInt(streamer_files_num, 10),
        streamer_file_duration: parseInt(streamer_file_duration, 10),
        streamer_player_buffer_files_num: parseInt(streamer_player_buffer_files_num, 10),
        nmos_enabled: nmos_enabled,
        nmos_registry_autodiscovery: nmos_registry_autodiscovery,
        nmos_registry_address: nmos_registry_address,
        nmos_registry_port: parseInt(nmos_registry_port, 10),
        nmos_registry_query_port: parseInt(nmos_registry_query_port, 10),
        nmos_control_interface: nmos_control_interface,
        nmos_node_port: parseInt(nmos_node_port, 10),
        nmos_mdns_enabled: nmos_mdns_enabled,
      }),
      method: 'POST'
    }).catch(err => {
      toast.error('Config set failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getPTPConfig() {
    return this.doFetch(ptpConfig).catch(err => {
      toast.error('PTP config get failed: ' + err.message);
      return Promise.reject(Error(err.message));
    });
  }

  static setPTPConfig(domain, dscp) {
    return this.doFetch(ptpConfig, {
      body: JSON.stringify({
        domain: parseInt(domain, 10),
        dscp: parseInt(dscp, 10)
      }),
      method: 'POST'
    }).catch(err => {
      toast.error('PTP config set failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static addSource(id, enabled, name, io, max_samples_per_packet, codec, address, ttl, payload_type, dscp, refclk_ptp_traceable, map, is_edit) {
    return this.doFetch(source + '/' + id, {
      body: JSON.stringify({
        enabled: enabled,
        name: name,
        io: io,
        codec: codec,
        address: address,
        map: map,
        max_samples_per_packet: parseInt(max_samples_per_packet, 10),
        ttl: parseInt(ttl, 10),
        payload_type: parseInt(payload_type, 10),
        dscp: parseInt(dscp, 10),
        refclk_ptp_traceable: refclk_ptp_traceable
      }),
      method: 'PUT'
    }).catch(err => {
      toast.error((is_edit ? 'Update Source failed: ' : 'Add Source failed: ') + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static removeSource(id) {
    return this.doFetch(source + '/' + id, {
      method: 'DELETE'
    }).catch(err => {
      toast.error('Remove Source failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getSourceSDP(id) {
    return this.doFetch(source +  sdp + '/' + id, {
      method: 'GET'
    }).catch(err => {
      toast.error('Get Source SDP failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getSinkStatus(id) {
    return this.doFetch(sink +  status + '/' + id, {
      method: 'GET'
    }).catch(err => {
      toast.error('Get Sink status failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getSourceStatus(id) {
    return this.doFetch(source + status + '/' + id, {
      method: 'GET'
    }).catch(err => {
      toast.error('Get Source status failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static addSink(id, name, io, delay, use_sdp, source, sdp, ignore_refclk_gmid, map, is_edit) {
    return this.doFetch(sink + '/' + id, {
      body: JSON.stringify({
        name: name,
        io: io,
        delay: parseInt(delay, 10),
        use_sdp: use_sdp,
        source: source,
        sdp: sdp,
        ignore_refclk_gmid: ignore_refclk_gmid,
        map: map
      }),
      method: 'PUT'
    }).catch(err => {
      toast.error((is_edit ? 'Update Sink failed: ' : 'Add Sink failed: ') + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static removeSink(id) {
    return this.doFetch(sink + '/' + id, {
      method: 'DELETE'
    }).catch(err => {
      toast.error('Remove Sink failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getPTPStatus() {
    return this.doFetch(ptpStatus).catch(err => {
      toast.error('PTP status get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getSources() {
    return this.doFetch(sources).catch(err => {
      toast.error('Sources get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getSinks() {
    return this.doFetch(sinks).catch(err => {
      toast.error('Sinks get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getStreams() {
    return this.doFetch(streams).catch(err => {
      toast.error('Streams get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getRemoteSources() {
    return this.doFetch(browseSources).catch(err => {
      toast.error('Browse sources get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  // ---- IS-08 (Audio Channel Mapping) ----

  static getChannelMapInputs() {
    return this.doFetchRaw(channelMapping + '/inputs/').catch(err => {
      toast.error('Channel map inputs get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapInputParent(id) {
    return this.doFetchRaw(channelMapping + '/inputs/' + id + '/parent/').catch(err => {
      toast.error('Channel map input parent get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapInputProperties(id) {
    return this.doFetchRaw(channelMapping + '/inputs/' + id + '/properties/').catch(err => {
      toast.error('Channel map input properties get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapInputChannels(id) {
    return this.doFetchRaw(channelMapping + '/inputs/' + id + '/channels/').catch(err => {
      toast.error('Channel map input channels get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapOutputs() {
    return this.doFetchRaw(channelMapping + '/outputs/').catch(err => {
      toast.error('Channel map outputs get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapOutputCaps(id) {
    return this.doFetchRaw(channelMapping + '/outputs/' + id + '/caps/').catch(err => {
      toast.error('Channel map output caps get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapOutputChannels(id) {
    return this.doFetchRaw(channelMapping + '/outputs/' + id + '/channels/').catch(err => {
      toast.error('Channel map output channels get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapOutputProperties(id) {
    return this.doFetchRaw(channelMapping + '/outputs/' + id + '/properties/').catch(err => {
      toast.error('Channel map output properties get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapOutputSourceId(id) {
    return this.doFetchRaw(channelMapping + '/outputs/' + id + '/sourceid/').catch(err => {
      toast.error('Channel map output source id get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static getChannelMapActive() {
    return this.doFetchRaw(channelMapping + '/map/active/').catch(err => {
      toast.error('Channel map active get failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

  static setChannelMapActivation(outputId, outputChannel, inputId, inputChannel) {
    return this.doFetchRaw(channelMapping + '/map/activations/', {
      body: JSON.stringify({
        activation: { mode: 'activate_immediate' },
        action: {
          [outputId]: {
            [outputChannel]: {
              input: inputId,
              channel_index: inputId === null ? null : inputChannel
            }
          }
        }
      }),
      method: 'POST'
    }).catch(err => {
      toast.error('Channel map activation failed: ' + err.message)
      return Promise.reject(Error(err.message));
    });
  }

}
