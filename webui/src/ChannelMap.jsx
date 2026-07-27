//
//  ChannelMap.jsx
//
//  IS-08 (Audio Channel Mapping) grid: one row per Output channel (a
//  Sender's Tx channel, or a raw ALSA playback channel), with a dropdown
//  listing that Output's routable Inputs (Receivers and/or raw ALSA
//  capture channels, per the Output's own /caps). Drives the real
//  /x-nmos/channelmapping/v1.0/ API directly (RestAPI.doFetchRaw), not a
//  daemon-proprietary shortcut.
//
//  Each change tears down and recreates the underlying RTP stream (see
//  nmos_is08.cpp) — not a cheap operation — so this intentionally uses a
//  plain <select> per row rather than free-form drag-and-drop.
//

import React, {Component} from 'react';
import RestAPI from './Services';
import Loader from './Loader';

function stripId(entry) {
  // Input/Output list entries look like "<uuid>/".
  return entry.replace(/\/$/, '');
}

class ChannelMap extends Component {
  constructor(props) {
    super(props);
    this.state = {
      inputLabels: {},
      outputs: [],
      active: {},
      isLoading: false,
    };
    this.fetchAll = this.fetchAll.bind(this);
    this.onChangeMapping = this.onChangeMapping.bind(this);
  }

  fetchAll() {
    this.setState({isLoading: true});

    const inputsP = RestAPI.getChannelMapInputs().then(r => r.json()).catch(() => []);
    const outputsP = RestAPI.getChannelMapOutputs().then(r => r.json()).catch(() => []);
    const activeP = RestAPI.getChannelMapActive().then(r => r.json()).catch(() => ({map: {}}));

    Promise.all([inputsP, outputsP, activeP]).then(([inputEntries, outputEntries, activeData]) => {
      const inputIds = inputEntries.map(stripId);
      const outputIds = outputEntries.map(stripId);

      const inputLabelsP = Promise.all(inputIds.map(id =>
        RestAPI.getChannelMapInputProperties(id)
          .then(r => r.json())
          .then(p => [id, p.name])
          .catch(() => [id, id])
      ));

      const outputInfoP = Promise.all(outputIds.map(id =>
        Promise.all([
          RestAPI.getChannelMapOutputProperties(id).then(r => r.json()).catch(() => ({name: id})),
          RestAPI.getChannelMapOutputCaps(id).then(r => r.json()).catch(() => ({routable_inputs: []})),
          RestAPI.getChannelMapOutputChannels(id).then(r => r.json()).catch(() => [{}]),
        ]).then(([props, caps, channels]) => ({
          id,
          label: props.name,
          routableInputs: (caps.routable_inputs || []).filter(x => x !== null),
          channelCount: channels.length || 1,
        }))
      ));

      Promise.all([inputLabelsP, outputInfoP]).then(([labelPairs, outputInfos]) => {
        const inputLabels = {};
        labelPairs.forEach(([id, label]) => { inputLabels[id] = label; });
        this.setState({
          inputLabels,
          outputs: outputInfos,
          active: activeData.map || {},
          isLoading: false,
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

  onChangeMapping(outputId, channel, inputId) {
    RestAPI.setChannelMapActivation(outputId, channel, inputId === '' ? null : inputId, 0)
      .then(this.fetchAll);
  }

  render() {
    if (this.state.isLoading && this.state.outputs.length === 0)
      return <Loader/>;

    return (
      <div id='channelmap'>
        <h3>Channel Map</h3>
        <table className='table-stream'><tbody>
          <tr className='tr-stream'>
            <th>Output</th><th>Ch</th><th>Input</th>
          </tr>
          {this.state.outputs.flatMap(o => {
            const activeOut = this.state.active[o.id] || {};
            return Array.from({length: o.channelCount}, (_, idx) => String(idx)).map(ch => {
              const current = activeOut[ch] || {input: null};
              return (
                <tr key={o.id + ':' + ch} className='tr-stream'>
                  <td>{o.label}</td>
                  <td>{ch}</td>
                  <td>
                    <select value={current.input || ''}
                      onChange={e => this.onChangeMapping(o.id, ch, e.target.value)}>
                      <option value=''>(none)</option>
                      {o.routableInputs.map(inId => (
                        <option key={inId} value={inId}>
                          {this.state.inputLabels[inId] || inId}
                        </option>
                      ))}
                    </select>
                  </td>
                </tr>
              );
            });
          })}
        </tbody></table>
        <span className='pointer-area' onClick={this.fetchAll}>
          <img width='30' height='30' src='/reload.png' alt=''/>
        </span>
      </div>
    );
  }
}

export default ChannelMap;
