//
//  PTP.jsx
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

import React, {Component} from 'react';
import PropTypes from 'prop-types';
import {toast} from 'react-toastify';

import RestAPI from './Services';
import Loader from './Loader';

function fmtAge(sec) {
  if (sec < 60) return `${sec}s`;
  return `${Math.round(sec / 60)}m`;
}


class PTPConfig extends Component {
  static propTypes = {
    domain: PropTypes.number.isRequired,
    dscp: PropTypes.number.isRequired,
  };

  constructor(props) {
    super(props);
    this.state = {
      domain: this.props.domain,
      dscp: this.props.dscp,
      domainErr: false,
    };
    this.onSubmit = this.onSubmit.bind(this);
    this.inputIsValid = this.inputIsValid.bind(this);
  }

  inputIsValid() {
    return !this.state.domainErr;
  }

  onSubmit(event) {
    event.preventDefault();
    RestAPI.setPTPConfig(this.state.domain, this.state.dscp)
      .then(response => toast.success('PTP config updated'));
  }

  render() {
    return (
     <div>
      <h3>Config</h3>
      <table><tbody>
        <tr>
          <th align="left"> <label>Type</label> </th>
          <th align="left"> <label>PTPv2</label> </th>
        </tr>
        <tr>
          <th align="left"> <label>Domain</label> </th>
          <th align="left">
            <select value={[0, 127].includes(+this.state.domain) ? +this.state.domain : 'custom'}
                    onChange={e => e.target.value !== 'custom' && this.setState({domain: +e.target.value, domainErr: false})}>
              <option value={0}>0 — IEEE standard</option>
              <option value={127}>127 — broadcast/AV</option>
              {![0, 127].includes(+this.state.domain) && <option value="custom">{this.state.domain} — custom</option>}
            </select>
            {![0, 127].includes(+this.state.domain) &&
              <input type='number' min='0' max='127' className='input-number'
                style={{marginLeft: '6px'}}
                value={this.state.domain}
                onChange={e => this.setState({domain: e.target.value, domainErr: !e.currentTarget.checkValidity()})}
                required/>}
          </th>
        </tr>
        <tr>
          <th align="left"> <label>DSCP</label> </th>
          <th align="left">
            <select value={this.state.dscp} onChange={e => this.setState({dscp: e.target.value})}>
              <option value="56">56 (CS7)</option>
              <option value="48">48 (CS6)</option>
              <option value="46">46 (EF)</option>
              <option value="36">36 (AF42)</option>
              <option value="34">34 (AF41)</option>
              <option value="0">0 (BE)</option>
            </select>
          </th>
        </tr>
        <tr>
          <th> <button disabled={this.inputIsValid() ? undefined : true} onClick={this.onSubmit}>Submit</button> </th>
        </tr>
      </tbody></table>
     </div>
    )
  }
}

class PTPStatus extends Component {
  static propTypes = {
    status: PropTypes.string.isRequired,
    gmid: PropTypes.string.isRequired,
    jitter: PropTypes.number.isRequired,
    jitterHistory: PropTypes.array.isRequired,
    activeLeg: PropTypes.number.isRequired,
    leg0Status: PropTypes.string.isRequired,
    leg1Status: PropTypes.string.isRequired,
    leg0Gmid: PropTypes.string.isRequired,
    leg1Gmid: PropTypes.string.isRequired,
    legsAligned: PropTypes.bool.isRequired,
  };

  constructor(props) {
    super(props);
    this.canvasRef = React.createRef();
  }

  drawChart() {
    const cv = this.canvasRef.current;
    if (!cv) return;
    const hist = this.props.jitterHistory;
    if (hist.length < 2) return;

    const dpr = window.devicePixelRatio || 1;
    const W = cv.clientWidth || 380, H = cv.clientHeight || 90;
    cv.width = W * dpr; cv.height = H * dpr;
    const ctx = cv.getContext('2d');
    ctx.scale(dpr, dpr);

    const isDark = document.documentElement.getAttribute('data-theme') === 'dark' ||
      (!document.documentElement.getAttribute('data-theme') &&
       window.matchMedia('(prefers-color-scheme: dark)').matches);
    const p = isDark
      ? { bg: '#1a1f2e', text: '#9ca3af', grid: 'rgba(255,255,255,0.06)', axis: '#374151', line: '#60a5fa', fill: 'rgba(96,165,250,0.13)' }
      : { bg: '#f3f4f6', text: '#6b7280', grid: 'rgba(0,0,0,0.07)', axis: '#d1d5db', line: '#1967a8', fill: 'rgba(25,103,168,0.10)' };

    ctx.fillStyle = p.bg;
    ctx.fillRect(0, 0, W, H);

    const mL = 46, mR = 6, mT = 7, mB = 18;
    const cW = W - mL - mR, cH = H - mT - mB;

    const min = Math.min(...hist), max = Math.max(...hist);
    const span = max - min || 1;
    const toY = v => mT + (1 - (v - min) / span) * cH;
    const toX = i => mL + (i / (hist.length - 1)) * cW;

    // Y grid + ticks + labels (3 levels)
    ctx.font = '9px monospace';
    const yTicks = [min, (min + max) / 2, max];
    yTicks.forEach(v => {
      const y = toY(v);
      ctx.strokeStyle = p.grid; ctx.lineWidth = 1;
      ctx.beginPath(); ctx.moveTo(mL, y); ctx.lineTo(mL + cW, y); ctx.stroke();
      ctx.strokeStyle = p.axis;
      ctx.beginPath(); ctx.moveTo(mL - 3, y); ctx.lineTo(mL, y); ctx.stroke();
      ctx.fillStyle = p.text; ctx.textAlign = 'right'; ctx.textBaseline = 'middle';
      ctx.fillText(Math.round(v), mL - 6, y);
    });

    // X ticks + labels
    const POLL_S = 5;
    const xIdxs = hist.length > 4
      ? [0, Math.floor((hist.length - 1) / 2), hist.length - 1]
      : [0, hist.length - 1];
    xIdxs.forEach((idx, i) => {
      const x = toX(idx);
      const ageSec = (hist.length - 1 - idx) * POLL_S;
      const label = ageSec === 0 ? 'now' : `-${fmtAge(ageSec)}`;
      ctx.strokeStyle = p.axis; ctx.lineWidth = 1;
      ctx.beginPath(); ctx.moveTo(x, mT + cH); ctx.lineTo(x, mT + cH + 3); ctx.stroke();
      ctx.fillStyle = p.text; ctx.textBaseline = 'top';
      ctx.textAlign = i === 0 ? 'left' : (i === xIdxs.length - 1 ? 'right' : 'center');
      ctx.fillText(label, x, mT + cH + 4);
    });

    // Axes
    ctx.strokeStyle = p.axis; ctx.lineWidth = 1;
    ctx.beginPath();
    ctx.moveTo(mL, mT); ctx.lineTo(mL, mT + cH); ctx.lineTo(mL + cW, mT + cH);
    ctx.stroke();

    // Area fill
    const pts = hist.map((v, i) => ({ x: toX(i), y: toY(v) }));
    ctx.beginPath();
    ctx.moveTo(pts[0].x, mT + cH);
    pts.forEach(p2 => ctx.lineTo(p2.x, p2.y));
    ctx.lineTo(pts[pts.length - 1].x, mT + cH);
    ctx.closePath();
    ctx.fillStyle = p.fill; ctx.fill();

    // Line
    ctx.beginPath();
    pts.forEach((p2, i) => i ? ctx.lineTo(p2.x, p2.y) : ctx.moveTo(p2.x, p2.y));
    ctx.strokeStyle = p.line; ctx.lineWidth = 1.5; ctx.lineJoin = 'round';
    ctx.stroke();
  }

  componentDidUpdate() { this.drawChart(); }
  componentDidMount()  { this.drawChart(); }

  render() {
    const locked  = this.props.status === 'locked';
    const locking = this.props.status === 'locking';
    const stColor = locked ? '#16a34a' : locking ? '#b45309' : '#b91c1c';
    const legColor = s => s === 'locked' ? '#16a34a' : s === 'locking' ? '#b45309' : '#b91c1c';
    return (
     <div>
      <h3>Status</h3>
      <table><tbody>
        <tr>
          <th align="left"> <label>Mode</label> </th>
          <th align="left"> <input value='Slave' disabled/> </th>
        </tr>
        <tr>
          <th align="left"> <label>Status</label> </th>
          <th align="left">
            <input value={this.props.status} disabled
              style={{color: stColor, fontWeight: 600}}/>
          </th>
        </tr>
        <tr>
          <th align="left"> <label>GMID</label> </th>
          <th align="left"> <input value={this.props.gmid} disabled style={{fontFamily: 'monospace', width: '16em'}}/> </th>
        </tr>
        <tr>
          <th align="left"> <label>Jitter (ns)</label> </th>
          <th align="left"> <input value={this.props.jitter} disabled/> </th>
        </tr>
        <tr>
          <th align="left"> <label>Active leg</label> </th>
          <th align="left">
            <input value={this.props.activeLeg === 0 ? 'Red (primary)' : 'Blue (secondary)'} disabled/>
          </th>
        </tr>
        <tr>
          <th align="left"> <label>Red leg</label> </th>
          <th align="left">
            <input value={this.props.leg0Status} disabled style={{color: legColor(this.props.leg0Status), fontWeight: 600, width: '6em'}}/>
            <input value={this.props.leg0Gmid} disabled style={{fontFamily: 'monospace', width: '16em', marginLeft: '6px'}}/>
          </th>
        </tr>
        <tr>
          <th align="left"> <label>Blue leg</label> </th>
          <th align="left">
            <input value={this.props.leg1Status} disabled style={{color: legColor(this.props.leg1Status), fontWeight: 600, width: '6em'}}/>
            <input value={this.props.leg1Gmid} disabled style={{fontFamily: 'monospace', width: '16em', marginLeft: '6px'}}/>
          </th>
        </tr>
      </tbody></table>
      {!this.props.legsAligned && (
        <p style={{color: '#b91c1c', fontWeight: 600, marginTop: '8px'}}>
          ⚠ Red and Blue are locked to different grandmasters - seamless 2022-7 switching cannot be trusted until they match.
        </p>
      )}
      {this.props.jitterHistory.length > 0 && (
        <div style={{marginTop: '8px'}}>
          <small style={{color: '#888'}}>Jitter history · {this.props.jitterHistory.length} samples</small>
          <canvas ref={this.canvasRef} width="380" height="90"
            style={{display: 'block', width: '380px', height: '90px', marginTop: '4px', borderRadius: '4px'}}/>
          {this.props.jitterHistory.length < 2 && <small style={{color: '#bbb', display: 'block'}}>collecting…</small>}
        </div>
      )}
     </div>
    )
  }
}

class PTP extends Component {
  constructor(props) {
    super(props);
    this.state = {
      domain: 0,
      domainErr: false,
      dscp: 0,
      status: '',
      gmid: '',
      jitter: 0,
      activeLeg: 0,
      leg0Status: 'unlocked',
      leg1Status: 'unlocked',
      leg0Gmid: '',
      leg1Gmid: '',
      legsAligned: true,
      jitterHistory: [],
      isConfigLoading: false,
      isStatusLoading: false,
    };
  }

  fetchStatus() {
    this.setState({isStatusLoading: true});
    RestAPI.getPTPStatus()
      .then(response => response.json())
      .then(
        data => {
          const jitter = parseInt(data.jitter, 10);
          this.setState(prev => ({
            status: data.status,
            gmid: data.gmid,
            jitter,
            activeLeg: parseInt(data.active_leg, 10) || 0,
            leg0Status: data.leg0_status,
            leg1Status: data.leg1_status,
            leg0Gmid: data.leg0_gmid,
            leg1Gmid: data.leg1_gmid,
            legsAligned: data.legs_aligned,
            jitterHistory: [...prev.jitterHistory.slice(-59), jitter],
            isStatusLoading: false
          }));
        })
      .catch(err => this.setState({isStatusLoading: false}));
  }

  fetchConfig() {
    this.setState({isConfigLoading: true});
    RestAPI.getPTPConfig()
      .then(response => response.json())
      .then(
        data => this.setState({
          domain: parseInt(data.domain, 10),
          dscp: parseInt(data.dscp, 10),
          isConfigLoading: false
        }))
      .catch(err => this.setState({isConfigLoading: false}));
  }

  componentDidMount() {
    this.fetchStatus();
    this.fetchConfig();
    this.interval = setInterval(() => { this.fetchStatus() }, 5000)
  }

  componentWillUnmount() {
    clearInterval(this.interval);
  }

  render() {
    return (
      <div className='ptp'>
        { this.state.isConfigLoading ? <Loader/> :
           <PTPConfig domain={this.state.domain} dscp={this.state.dscp}/> }
        <br/>
        { (this.state.isStatusLoading && this.state.jitterHistory.length === 0) ? <Loader/> :
           <PTPStatus status={this.state.status} gmid={this.state.gmid}
             jitter={this.state.jitter} jitterHistory={this.state.jitterHistory}
             activeLeg={this.state.activeLeg}
             leg0Status={this.state.leg0Status} leg1Status={this.state.leg1Status}
             leg0Gmid={this.state.leg0Gmid} leg1Gmid={this.state.leg1Gmid}
             legsAligned={this.state.legsAligned}/> }
      </div>
    )
  }
}

export default PTP;
