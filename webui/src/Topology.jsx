//
//  Topology.jsx
//
//  Shows active audio routing: local sources, remote sources, pipes
//  (sink→source connections), and unconnected resources.
//

import React, {Component} from 'react';
import RestAPI from './Services';
import Loader from './Loader';

function extractIp(url) {
  // Pull bare IP from rtsp://x.x.x.x:port/... or sdp:// or udp://
  const m = url.match(/\/\/([\d.]+)/);
  return m ? m[1] : url;
}

function statusBadge(flags) {
  if (!flags) return <span className='topo-badge topo-unknown'>?</span>;
  if (flags.receiving_rtp_packet)
    return <span className='topo-badge topo-active'>● receiving</span>;
  if (flags.muted)
    return <span className='topo-badge topo-muted'>◌ muted</span>;
  return <span className='topo-badge topo-wait'>○ waiting</span>;
}

class Topology extends Component {
  constructor(props) {
    super(props);
    this.state = {
      localSources:  [],
      remoteSources: [],
      sinks:         [],
      sinkStatuses:  {},
      isLoading:     false,
    };
    this.fetchAll = this.fetchAll.bind(this);
  }

  fetchAll() {
    this.setState({ isLoading: true });

    const p = [
      RestAPI.getSources().then(r => r.json()).catch(() => ({ sources: [] })),
      RestAPI.getSinks().then(r => r.json()).catch(() => ({ sinks: [] })),
      RestAPI.getRemoteSources().then(r => r.json()).catch(() => ({ remote_sources: [] })),
    ];

    Promise.all(p).then(([srcData, snkData, remData]) => {
      const localSources  = srcData.sources       || [];
      const sinks         = snkData.sinks         || [];
      const remoteSources = remData.remote_sources || [];

      const statusPs = sinks.map(sink =>
        RestAPI.getSinkStatus(sink.id)
          .then(r => r.json())
          .then(s => [sink.id, s])
          .catch(() => [sink.id, null])
      );

      Promise.all(statusPs).then(pairs => {
        const sinkStatuses = {};
        pairs.forEach(([id, s]) => { sinkStatuses[id] = s; });
        this.setState({ localSources, remoteSources, sinks, sinkStatuses, isLoading: false });
      });
    }).catch(() => this.setState({ isLoading: false }));
  }

  componentDidMount() {
    this.fetchAll();
    this.interval = setInterval(this.fetchAll, 5000);
  }

  componentWillUnmount() {
    clearInterval(this.interval);
  }

  // Derive pipes and free resources from state
  buildView() {
    const { localSources, remoteSources, sinks, sinkStatuses } = this.state;

    const connectedLocalSrcIds  = new Set();
    const connectedRemoteSrcIds = new Set();
    const connectedSinkIds      = new Set();
    const pipes = [];

    for (const sink of sinks) {
      if (!sink.source) continue;
      const sinkIp = extractIp(sink.source);

      const localMatch  = localSources.find(s => s.address === sinkIp);
      const remoteMatch = !localMatch && remoteSources.find(s => s.address === sinkIp);
      const status      = sinkStatuses[sink.id];

      pipes.push({
        sink,
        source:    localMatch || remoteMatch || null,
        sourceUrl: sink.source,
        isLocal:   !!localMatch,
        flags:     status ? status.sink_flags : null,
        minTime:   status ? status.sink_min_time : null,
      });

      connectedSinkIds.add(sink.id);
      if (localMatch)  connectedLocalSrcIds.add(localMatch.id);
      if (remoteMatch) connectedRemoteSrcIds.add(remoteMatch.id);
    }

    const idleLocalSources  = localSources.filter(s => !connectedLocalSrcIds.has(s.id));
    const idleRemoteSources = remoteSources.filter(s => !connectedRemoteSrcIds.has(s.id));
    const idleSinks         = sinks.filter(s => !connectedSinkIds.has(s.id));

    return { pipes, idleLocalSources, idleRemoteSources, idleSinks };
  }

  render() {
    if (this.state.isLoading && this.state.sinks.length === 0)
      return <Loader/>;

    const { pipes, idleLocalSources, idleRemoteSources, idleSinks } = this.buildView();

    return (
      <div id='topology'>

        {/* ── Active pipes ── */}
        <div className='topo-section'>
          <h3>Active Connections</h3>
          {pipes.length === 0
            ? <p className='topo-empty'>No sinks are connected to a source.</p>
            : (
              <table className='table-stream'><tbody>
                <tr className='tr-stream'>
                  <th>Source</th>
                  <th></th>
                  <th>Sink</th>
                  <th>Status</th>
                </tr>
                {pipes.map((p, i) => (
                  <tr key={i} className='tr-stream'>
                    <td>
                      <strong>{p.source ? p.source.name : extractIp(p.sourceUrl)}</strong>
                      &nbsp;
                      <span className={'topo-badge ' + (p.isLocal ? 'topo-local' : 'topo-remote')}>
                        {p.isLocal ? 'local' : 'remote'}
                      </span>
                      <br/>
                      <small>{p.source ? p.source.address : p.sourceUrl}</small>
                      {p.source && <small> · {p.source.codec || ''} · {p.sink.map.length}ch</small>}
                    </td>
                    <td className='topo-arrow'>→</td>
                    <td>
                      <strong>{p.sink.name}</strong><br/>
                      <small>{p.sink.io} · {p.sink.delay} ms</small>
                    </td>
                    <td>{statusBadge(p.flags)}</td>
                  </tr>
                ))}
              </tbody></table>
            )
          }
        </div>

        {/* ── Idle local sources ── */}
        {idleLocalSources.length > 0 && (
          <div className='topo-section'>
            <h3>Local Sources — not received by any sink</h3>
            <table className='table-stream'><tbody>
              <tr className='tr-stream'>
                <th>ID</th><th>Name</th><th>Address</th><th>Codec</th><th>Ch</th>
              </tr>
              {idleLocalSources.map(s => (
                <tr key={s.id} className='tr-stream'>
                  <td>{s.id}</td>
                  <td>{s.name}</td>
                  <td>{s.address}</td>
                  <td>{s.codec}</td>
                  <td>{s.map.length}</td>
                </tr>
              ))}
            </tbody></table>
          </div>
        )}

        {/* ── Idle remote sources ── */}
        {idleRemoteSources.length > 0 && (
          <div className='topo-section'>
            <h3>Remote Sources — visible but not connected</h3>
            <table className='table-stream'><tbody>
              <tr className='tr-stream'>
                <th>Name</th><th>Address</th><th>Domain</th><th>Last seen</th>
              </tr>
              {idleRemoteSources.map((s, i) => (
                <tr key={i} className='tr-stream'>
                  <td>{s.name}</td>
                  <td>{s.address}</td>
                  <td>{s.domain}</td>
                  <td>{s.last_seen}s ago</td>
                </tr>
              ))}
            </tbody></table>
          </div>
        )}

        {/* ── Idle sinks ── */}
        {idleSinks.length > 0 && (
          <div className='topo-section'>
            <h3>Sinks — not connected</h3>
            <table className='table-stream'><tbody>
              <tr className='tr-stream'>
                <th>ID</th><th>Name</th><th>Device</th><th>Ch</th>
              </tr>
              {idleSinks.map(s => (
                <tr key={s.id} className='tr-stream'>
                  <td>{s.id}</td>
                  <td>{s.name}</td>
                  <td>{s.io}</td>
                  <td>{s.map.length}</td>
                </tr>
              ))}
            </tbody></table>
          </div>
        )}

        <span className='pointer-area' onClick={this.fetchAll}>
          <img width='30' height='30' src='/reload.png' alt=''/>
        </span>
      </div>
    );
  }
}

export default Topology;
