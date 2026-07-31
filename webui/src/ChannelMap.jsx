//
//  ChannelMap.jsx
//
//  IS-08 (Audio Channel Mapping) grid, with the same 5 columns (Stream Rx,
//  Leg, Capture, Playback, Stream Tx) presented two ways:
//
//  - ALSA view: one row per physical ALSA channel number, the daemon's real
//    pivot point - a Sink's map[] entry and a Source's map[] entry are
//    independent facts that just happen to reference the same ALSA channel
//    number. Both "Stream Rx" and "Stream Tx" are dropdowns here (pick which
//    stream channel uses this ALSA channel); "Capture"/"Playback" are just
//    that row's fixed ALSA identity.
//  - Stream view: one row per real Rx/Tx audio channel instead - the mirror
//    image. "Stream Rx"/"Stream Tx" are now the row's fixed identity, and
//    "Capture"/"Playback" become the dropdown (pick which ALSA channel this
//    stream channel uses). Same underlying data, same activation calls,
//    just which side is "the row" and which is "the dropdown" is flipped.
//
//  Drives the real /x-nmos/channelmapping/v1.0/ API directly
//  (RestAPI.doFetchRaw), not a daemon-proprietary shortcut. Each change
//  tears down and recreates the underlying RTP stream (see nmos_is08.cpp)
//  - not a cheap operation - so this intentionally uses a plain <select>
//  per row rather than free-form drag-and-drop.
//

import React, {Component} from 'react';
import RestAPI from './Services';
import Loader from './Loader';

function stripId(entry) {
  // Input/Output list entries look like "<uuid>/".
  return entry.replace(/\/$/, '');
}

// SMPTE 2022-7 leg badge for a Receiver-backed Input currently feeding a
// row — sourced from the same sink_status the Topology tab already uses
// (extended with a "leg2" block), not a new IS-12-specific endpoint.
function legBadge(sinkStatus) {
  if (!sinkStatus) return null;
  const primary = !!(sinkStatus.sink_flags && sinkStatus.sink_flags.receiving_rtp_packet);
  const leg2 = sinkStatus.leg2 || {};
  if (!leg2.present) {
    return primary
      ? <span className='topo-badge topo-active'>red ●</span>
      : <span className='topo-badge topo-wait'>red ○</span>;
  }
  if (primary && leg2.receiving_rtp_packet)
    return <span className='topo-badge topo-active'>red+blue ●</span>;
  if (primary)
    return <span className='topo-badge topo-muted'>red only</span>;
  if (leg2.receiving_rtp_packet)
    return <span className='topo-badge topo-muted'>blue only</span>;
  return <span className='topo-badge topo-wait'>no signal</span>;
}

// Same idea, Tx direction (source_status_to_json's shape) - for Stream-view
// rows keyed by a Sender's own Tx channel rather than a Sink's Rx channel.
function txLegBadge(sourceStatus) {
  if (!sourceStatus) return null;
  const primary = !!(sourceStatus.source_flags && sourceStatus.source_flags.transmitting);
  const leg2 = sourceStatus.leg2 || {};
  if (!leg2.present) {
    return primary
      ? <span className='topo-badge topo-active'>red ●</span>
      : <span className='topo-badge topo-wait'>red ○</span>;
  }
  if (primary && leg2.transmitting)
    return <span className='topo-badge topo-active'>red+blue ●</span>;
  if (primary)
    return <span className='topo-badge topo-muted'>red only</span>;
  if (leg2.transmitting)
    return <span className='topo-badge topo-muted'>blue only</span>;
  return <span className='topo-badge topo-wait'>no signal</span>;
}

class ChannelMap extends Component {
  constructor(props) {
    super(props);
    this.state = {
      view: 'alsa',  // 'alsa' (row per ALSA channel) or 'stream' (row per real Rx/Tx channel)
      channels: [],       // sorted list of physical ALSA channel numbers
      captureUuid: {},    // channel number -> "ALSA Capture N" input uuid
      playbackUuid: {},   // channel number -> "ALSA Playback N" output uuid
      inputSinkIds: {},   // input uuid -> daemon sink id, for Receiver-backed Inputs only
      // input uuid -> [{value: "<uuid>::<channel index>", label}, ...] - one
      // entry per real channel of that input (e.g. 8 for an 8-channel Sink).
      inputOptions: {},
      // Flattened list of every real Stream Tx channel, shared by every row's
      // capture-side dropdown: {value: "<output uuid>::<channel>", label}.
      txOptions: [],
      // Same list as the Stream view's "Capture" dropdown offers: every raw
      // ALSA Capture channel plus every Sink-repeater option (inputOptions).
      captureOptions: [],
      playbackSelection: {},  // channel number -> currently selected inputOptions value ('' if none)
      captureSelection: {},   // channel number -> currently selected txOptions value ('' if none)
      // Stream view row lists - one entry per real Rx/Tx audio channel.
      rxChannels: [],  // [{value, label, sinkId, alsaChannel}]
      txChannels: [],  // [{value, label, sourceId, alsaChannel}]
      sinkStatuses: {},   // sink id -> /api/sink/status/{id} response
      sourceStatuses: {}, // source id -> /api/source/status/{id} response
      isLoading: false,
    };
    this.fetchAll = this.fetchAll.bind(this);
    this.onChangePlayback = this.onChangePlayback.bind(this);
    this.onChangeCapture = this.onChangeCapture.bind(this);
    this.onChangeTxInput = this.onChangeTxInput.bind(this);
  }

  fetchAll() {
    this.setState({isLoading: true});

    const inputsP = RestAPI.getChannelMapInputs().then(r => r.json()).catch(() => []);
    const outputsP = RestAPI.getChannelMapOutputs().then(r => r.json()).catch(() => []);
    const sinksP = RestAPI.getSinks().then(r => r.json()).catch(() => ({sinks: []}));
    const sourcesP = RestAPI.getSources().then(r => r.json()).catch(() => ({sources: []}));

    Promise.all([inputsP, outputsP, sinksP, sourcesP]).then(
        ([inputEntries, outputEntries, sinksData, sourcesData]) => {
      const inputIds = inputEntries.map(stripId);
      const outputIds = outputEntries.map(stripId);
      const sinksByName = {};
      const sinkStreamNames = {};  // sink id -> connected stream's real SDP session name
      const sinkMaps = {};         // sink id -> its real, raw map[] (ALSA playback channel per Rx channel)
      (sinksData.sinks || []).forEach(s => {
        sinksByName[s.name] = s.id;
        sinkMaps[s.id] = s.map;
        const m = (s.sdp || '').match(/(?:^|\r?\n)s=([^\r\n]+)/);
        if (m) sinkStreamNames[s.id] = m[1];
      });
      const sourcesByName = {};
      const sourceMaps = {};       // source id -> its real, raw map[] (ALSA capture channel per Tx channel)
      (sourcesData.sources || []).forEach(s => {
        sourcesByName[s.name] = s.id;
        sourceMaps[s.id] = s.map;
      });

      const inputInfoP = Promise.all(inputIds.map(id =>
        Promise.all([
          RestAPI.getChannelMapInputProperties(id).then(r => r.json()).catch(() => ({name: id})),
          RestAPI.getChannelMapInputChannels(id).then(r => r.json()).catch(() => [{}]),
        ]).then(([props, channels]) => ({id, label: props.name, channelLabels: channels.map(c => c.label)}))
      ));

      const outputInfoP = Promise.all(outputIds.map(id =>
        Promise.all([
          RestAPI.getChannelMapOutputProperties(id).then(r => r.json()).catch(() => ({name: id})),
          RestAPI.getChannelMapOutputChannels(id).then(r => r.json()).catch(() => [{}]),
        ]).then(([props, channels]) => ({
          id,
          label: props.name,
          channelCount: channels.length || 1,
          channelLabels: channels.map(c => c.label),
        }))
      ));

      Promise.all([inputInfoP, outputInfoP]).then(([inputInfos, outputInfos]) => {
        const inputSinkIds = {};
        const inputOptions = {};
        const captureUuid = {};
        const rxChannels = [];
        // sink id -> [value for channel 0, value for channel 1, ...] - lets
        // a raw sink.map[] scan below turn "(sinkId, channel k)" straight
        // into the dropdown value for that exact real audio channel.
        const sinkChannelValue = {};
        const streamRxPrefix = 'Stream Rx: ';
        const captureRe = /^ALSA Capture (\d+)$/;

        inputInfos.forEach(({id, label, channelLabels}) => {
          const captureMatch = label.match(captureRe);
          if (captureMatch) {
            // Recorded only as a capture-source identity for column 4 - an
            // ALSA Capture channel is never itself a valid choice for the
            // Stream Rx -> Playback column (no ALSA-to-ALSA routing exists).
            captureUuid[Number(captureMatch[1])] = id;
            return;
          }

          const sinkName = label.startsWith(streamRxPrefix) ? label.slice(streamRxPrefix.length) : label;
          const sinkId = sinksByName[sinkName];
          if (sinkId !== undefined) inputSinkIds[id] = sinkId;

          if (sinkId !== undefined && channelLabels.length > 1) {
            // A multichannel Sink is one Input resource but N real audio
            // channels - list each one directly instead of making the user
            // pick the resource first and its channel in a second step.
            const streamName = sinkStreamNames[sinkId];
            const base = 'Sink ' + sinkId + (streamName ? ' · ' + streamName : '');
            inputOptions[id] = channelLabels.map((chLabel, i) => ({
              value: id + '::' + i,
              label: base + ' · ' + chLabel,
            }));
            sinkChannelValue[sinkId] = inputOptions[id].map(opt => opt.value);
          } else {
            inputOptions[id] = [{value: id + '::0', label}];
            if (sinkId !== undefined) sinkChannelValue[sinkId] = [id + '::0'];
          }

          // Stream-view row: one per real Rx channel, regardless of whether
          // it's currently selected anywhere in the ALSA view.
          if (sinkId !== undefined) {
            const map = sinkMaps[sinkId] || [];
            inputOptions[id].forEach((opt, k) => {
              rxChannels.push({value: opt.value, label: opt.label, sinkId, alsaChannel: map[k]});
            });
          }
        });

        const playbackUuid = {};
        const txOptions = [];
        const txChannels = [];
        // source id -> [value for channel 0, value for channel 1, ...] -
        // same idea as sinkChannelValue, for a raw source.map[] scan.
        const sourceChannelValue = {};
        const playbackRe = /^ALSA Playback (\d+)$/;
        const streamTxPrefix = 'Stream Tx: ';

        outputInfos.forEach(o => {
          const playbackMatch = o.label.match(playbackRe);
          if (playbackMatch) {
            playbackUuid[Number(playbackMatch[1])] = o.id;
            return;
          }
          // Everything else is a Sender's Tx channel group - flatten into
          // one selectable entry per real channel, shared across every row.
          const sourceName = o.label.startsWith(streamTxPrefix) ? o.label.slice(streamTxPrefix.length) : o.label;
          const sourceId = sourcesByName[sourceName];
          const map = sourceId !== undefined ? (sourceMaps[sourceId] || []) : [];
          const values = o.channelLabels.map((chLabel, i) => {
            const value = o.id + '::' + i;
            const label = o.label + ' · ' + chLabel;
            txOptions.push({value, label});
            if (sourceId !== undefined) {
              txChannels.push({value, label, sourceId, alsaChannel: map[i]});
            }
            return value;
          });
          if (sourceId !== undefined) sourceChannelValue[sourceId] = values;
        });

        const channels = Object.keys(playbackUuid).map(Number).sort((a, b) => a - b);
        // Every option the Stream view's "Capture" dropdown can offer for a
        // Tx-channel row: a raw ALSA Capture channel, or (repeater) any real
        // Sink channel - the same choice the ALSA view's "Stream Tx" column
        // already exposes, just anchored to a fixed output row instead.
        const captureOptions = [
          ...Object.values(inputOptions).flat(),
          ...channels.map(n => ({value: captureUuid[n] + '::0', label: 'ALSA Capture ' + n})),
        ];

        // Column 1: which Sink channel's real map[] entry actually points at
        // this ALSA channel - the one, unambiguous way audio lands on a
        // playback channel (nothing else can write to it).
        const playbackSelection = {};
        channels.forEach(ch => { playbackSelection[ch] = ''; });
        Object.entries(sinkMaps).forEach(([sinkId, map]) => {
          (map || []).forEach((alsaCh, k) => {
            if (channels.includes(alsaCh) && sinkChannelValue[sinkId]) {
              playbackSelection[alsaCh] = sinkChannelValue[sinkId][k] || '';
            }
          });
        });

        // Column 4: which Source channel's real map[] entry actually reads
        // from this ALSA channel - independent of whether some Sink also
        // happens to write to the same channel number (that's column 1's
        // fact, not this one's).
        const captureSelection = {};
        channels.forEach(ch => { captureSelection[ch] = ''; });
        Object.entries(sourceMaps).forEach(([sourceId, map]) => {
          (map || []).forEach((alsaCh, i) => {
            if (channels.includes(alsaCh) && sourceChannelValue[sourceId]) {
              captureSelection[alsaCh] = sourceChannelValue[sourceId][i] || '';
            }
          });
        });

        // Every real Sink/Source is a row in the Stream view (regardless of
        // whether it's currently selected in the ALSA view), so just fetch
        // status for all of them - there are only ever a handful.
        const sinkIds = Object.keys(sinkMaps).map(Number);
        const sourceIds = Object.keys(sourceMaps).map(Number);

        Promise.all([
          Promise.all(sinkIds.map(id =>
            RestAPI.getSinkStatus(id).then(r => r.json()).then(s => [id, s]).catch(() => [id, null])
          )),
          Promise.all(sourceIds.map(id =>
            RestAPI.getSourceStatus(id).then(r => r.json()).then(s => [id, s]).catch(() => [id, null])
          )),
        ]).then(([sinkPairs, sourcePairs]) => {
          const sinkStatuses = {};
          sinkPairs.forEach(([id, s]) => { sinkStatuses[id] = s; });
          const sourceStatuses = {};
          sourcePairs.forEach(([id, s]) => { sourceStatuses[id] = s; });
          this.setState({
            channels,
            captureUuid,
            playbackUuid,
            inputSinkIds,
            inputOptions,
            txOptions,
            captureOptions,
            rxChannels,
            txChannels,
            playbackSelection,
            captureSelection,
            sinkStatuses,
            sourceStatuses,
            isLoading: false,
          });
        });
      });
    }).catch(() => this.setState({isLoading: false}));
  }

  componentDidMount() {
    this.fetchAll();
    this.interval = setInterval(this.fetchAll, 5000);
  }

  componentWillUnmount() {
    clearInterval(this.interval);
  }

  onChangePlayback(channel, value) {
    const outputId = this.state.playbackUuid[channel];
    if (value === '') {
      RestAPI.setChannelMapActivation(outputId, '0', null, 0).then(this.fetchAll);
      return;
    }
    const sep = value.lastIndexOf('::');
    const inputId = value.slice(0, sep);
    const inputChannel = Number(value.slice(sep + 2));
    RestAPI.setChannelMapActivation(outputId, '0', inputId, inputChannel).then(this.fetchAll);
  }

  onChangeCapture(channel, value) {
    // No real "clear" is possible here: a Sender's Tx channel always reads
    // from some ALSA capture channel (map[] has no null value), so picking
    // "(none)" can't be honored - see nmos_is08.cpp's is08_apply_action_json.
    if (value === '') return;
    const sep = value.lastIndexOf('::');
    const outputId = value.slice(0, sep);
    const outputChannel = value.slice(sep + 2);
    RestAPI.setChannelMapActivation(outputId, outputChannel, this.state.captureUuid[channel], 0)
      .then(this.fetchAll);
  }

  // Stream view's "Capture" dropdown for a Tx-channel row: the row (output)
  // is fixed, the input (ALSA Capture or Sink-repeater) is what's chosen -
  // the mirror image of onChangeCapture, whose row is the ALSA channel and
  // whose choice is which Tx channel to feed.
  onChangeTxInput(outputId, outputChannel, value) {
    if (value === '') return;  // same no-clear constraint as onChangeCapture
    const sep = value.lastIndexOf('::');
    const inputId = value.slice(0, sep);
    const inputChannel = Number(value.slice(sep + 2));
    RestAPI.setChannelMapActivation(outputId, outputChannel, inputId, inputChannel).then(this.fetchAll);
  }

  renderAlsaRows() {
    return this.state.channels.map(ch => {
      const playbackValue = this.state.playbackSelection[ch] || '';
      const captureValue = this.state.captureSelection[ch] || '';
      const inputUuid = playbackValue ? playbackValue.slice(0, playbackValue.lastIndexOf('::')) : null;
      const sinkId = inputUuid !== null ? this.state.inputSinkIds[inputUuid] : undefined;
      const sinkStatus = sinkId !== undefined ? this.state.sinkStatuses[sinkId] : null;
      return (
        <tr key={ch} className='tr-stream'>
          <td>
            <select value={playbackValue} onChange={e => this.onChangePlayback(ch, e.target.value)}>
              <option value=''>(none)</option>
              {Object.values(this.state.inputOptions).flatMap(opts =>
                opts.map(opt => <option key={opt.value} value={opt.value}>{opt.label}</option>)
              )}
            </select>
          </td>
          <td>{legBadge(sinkStatus)}</td>
          <td>ALSA Capture {ch}</td>
          <td>ALSA Playback {ch}</td>
          <td>
            <select value={captureValue} onChange={e => this.onChangeCapture(ch, e.target.value)}>
              <option value=''>(none)</option>
              {this.state.txOptions.map(opt => (
                <option key={opt.value} value={opt.value}>{opt.label}</option>
              ))}
            </select>
          </td>
        </tr>
      );
    });
  }

  renderStreamRows() {
    const rxRows = this.state.rxChannels.map(rx => (
      <tr key={'rx:' + rx.value} className='tr-stream'>
        <td>{rx.label}</td>
        <td>{legBadge(this.state.sinkStatuses[rx.sinkId])}</td>
        <td>—</td>
        <td>
          <select value={rx.alsaChannel}
            onChange={e => this.onChangePlayback(Number(e.target.value), rx.value)}>
            {this.state.channels.map(ch => (
              <option key={ch} value={ch}>ALSA Playback {ch}</option>
            ))}
          </select>
        </td>
        <td>—</td>
      </tr>
    ));
    const txRows = this.state.txChannels.map(tx => {
      // Prefer showing a Sink-repeater match when one exists - it's the same
      // stickiness onChangeCapture relies on in the ALSA view, just read the
      // other way around here (already computed in playbackSelection).
      const currentValue = this.state.playbackSelection[tx.alsaChannel] ||
        (this.state.captureUuid[tx.alsaChannel] + '::0');
      return (
        <tr key={'tx:' + tx.value} className='tr-stream'>
          <td>—</td>
          <td>{txLegBadge(this.state.sourceStatuses[tx.sourceId])}</td>
          <td>
            <select value={currentValue}
              onChange={e => {
                const sep = tx.value.lastIndexOf('::');
                this.onChangeTxInput(tx.value.slice(0, sep), tx.value.slice(sep + 2), e.target.value);
              }}>
              {this.state.captureOptions.map(opt => (
                <option key={opt.value} value={opt.value}>{opt.label}</option>
              ))}
            </select>
          </td>
          <td>—</td>
          <td>{tx.label}</td>
        </tr>
      );
    });
    return [...rxRows, ...txRows];
  }

  render() {
    if (this.state.isLoading && this.state.channels.length === 0)
      return <Loader/>;

    return (
      <div id='channelmap'>
        <h3>Channel Map</h3>
        <div style={{marginBottom: '8px'}}>
          <button disabled={this.state.view === 'alsa'}
            onClick={() => this.setState({view: 'alsa'})}>ALSA view</button>
          {' '}
          <button disabled={this.state.view === 'stream'}
            onClick={() => this.setState({view: 'stream'})}>Stream view</button>
        </div>
        <table className='table-stream'><tbody>
          <tr className='tr-stream'>
            <th>Stream Rx</th><th>Leg</th><th>Capture</th><th>Playback</th><th>Stream Tx</th>
          </tr>
          {this.state.view === 'alsa' ? this.renderAlsaRows() : this.renderStreamRows()}
        </tbody></table>
        <span className='pointer-area' onClick={this.fetchAll}>
          <img width='30' height='30' src='/reload.png' alt=''/>
        </span>
      </div>
    );
  }
}

export default ChannelMap;
