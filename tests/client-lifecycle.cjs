'use strict';
const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');
const script = fs.readFileSync('web/index.html', 'utf8').match(/<script>([\s\S]*?)<\/script>/)[1];
function deferred() {
 let resolve, reject;
 const promise = new Promise((yes, no) => {resolve = yes; reject = no;});
 return {promise, resolve, reject};
}
async function settle() {for (let i = 0; i < 30; i++) await Promise.resolve();}
function harness() {
 let now = 0, timerId = 0, frameId = 0, offerId = 0, focused = null;
 const captures = new Map();
 const timers = new Map(), callbacks = new Map(), elements = new Map();
 const listeners = new Map();
 const on = (target,type,fn) => {const key=target+':'+type; if(!listeners.has(key)) listeners.set(key,[]); listeners.get(key).push(fn);};
 const fire = (target,type,properties={}) => {const event={preventDefault(){},repeat:false,shiftKey:false,ctrlKey:false,altKey:false,metaKey:false,pointerId:1,button:0,clientX:300,clientY:200,...properties}; for(const fn of listeners.get(target+':'+type)||[]) fn(event);};
 const calls = [], channels = [], peers = [], clipboardWrites = [];
 const element = id => {
  if (!elements.has(id)) elements.set(id, {
   textContent:'', disabled:false, checked:true, hidden:false, srcObject:null, videoWidth:1920, videoHeight:1080,
   classList:{toggle() {}}, addEventListener(type,fn) {on(id,type,fn);}, focus() {const previous=focused;focused=id;if(previous&&previous!==id)fire(previous,'blur');}, play:async()=>{},
   getBoundingClientRect:()=>({left:0,top:0,width:1920,height:1080}),setPointerCapture(pointerId){if(env.captureFails)throw new Error('capture failed');captures.set(pointerId,id);},hasPointerCapture:pointerId=>captures.get(pointerId)===id,
   releasePointerCapture(pointerId){if(captures.get(pointerId)===id){captures.delete(pointerId);fire(id,'lostpointercapture',{pointerId});}},
   requestVideoFrameCallback(callback) {const id = ++frameId; callbacks.set(id, callback); return id;},
   cancelVideoFrameCallback(id) {callbacks.delete(id);},
  });
  return elements.get(id);
 };
 function schedule(callback, delay, interval = false) {
  const id = ++timerId; timers.set(id, {callback, at:now + delay, delay, interval}); return id;
 }
 const env = {
  fetch:async call => call.path === '/api/stop' ? {status:204, ok:true} : {status:200, ok:true, json:async()=>({sdp:'answer', lease:String(++offerId).padStart(64, '0'), epoch:`epoch-${offerId}`})},
  readText:async()=> 'dummy browser text',
  writeText:async text => {clipboardWrites.push(text);},
  codecs:[{mimeType:'video/H264', sdpFmtpLine:'packetization-mode=1;profile-level-id=42e01f'}],
 };
 class PC {
  constructor() {this.iceGatheringState = 'complete'; this.connectionState = 'new'; peers.push(this);}
  addTransceiver() {
   this.receiver = {jitterBufferTarget:50};
   return {receiver:this.receiver, setCodecPreferences:codecs=>{this.codecPreferences = Array.from(codecs);}};
  }
  createDataChannel() {
   const channel = {readyState:'connecting', bufferedAmount:0, sent:[], send(text) {this.sent.push(JSON.parse(text));}, close() {this.readyState = 'closed'; this.onclose?.();}};
   this.channel = channel; channels.push(channel); return channel;
  }
  async createOffer() {return {type:'offer', sdp:'offer'};}
  async setLocalDescription(offer) {this.localDescription = offer;}
  async setRemoteDescription() {this.channel.readyState = 'open'; this.channel.onopen?.(); this.ontrack?.({track:{}});env.onRemoteDescription?.();}
  close() {this.connectionState = 'closed'; this.onconnectionstatechange?.();}
  addEventListener() {}
  removeEventListener() {}
 }
 const context = vm.createContext({
  document:{hidden:false,getElementById:element, addEventListener(type,fn){on('document',type,fn);}}, window:{addEventListener(type,fn){on('window',type,fn);}},
  navigator:{clipboard:{readText:()=>env.readText(), writeText:text=>env.writeText(text)}},
  performance:{now:()=>now}, TextEncoder, AbortController, RTCPeerConnection:PC,
  RTCRtpReceiver:{getCapabilities:()=>({codecs:env.codecs})},
  MediaStream:class {constructor(tracks) {this.tracks = tracks;}},
  fetch:(path, options)=>{const call = {path, body:JSON.parse(options.body), signal:options.signal, at:now}; calls.push(call); return env.fetch(call);},
  setTimeout:(callback, delay)=>schedule(callback, delay), clearTimeout:id=>timers.delete(id),
  setInterval:(callback, delay)=>schedule(callback, delay, true), clearInterval:id=>timers.delete(id),
  requestAnimationFrame:callback=>schedule(callback, 16),
 });
 vm.runInContext(script, context, {filename:'web/index.html'});
 const app = context.window.tailWindowAccess;
 const message = body => app.state.channel.onmessage({data:JSON.stringify(body)});
 function ready() {
  message({type:'hello', epoch:app.state.epoch, input_revision:'0'});
  message({type:'frame', epoch:app.state.epoch, generation:1, rtp:42});
  const [id, callback] = callbacks.entries().next().value;
  callbacks.delete(id); callback(now, {rtpTimestamp:42});
  message({type:'state', input_revision:app.state.inputRevision, ready:true, clipboard_ready:true, generation:1, lock:'unlocked', bitrate_kbps:3000, fps:30});
  assert.equal(app.state.ready, true);
  assert.equal(app.state.generation, 1);
 }
 function present(rtp) {const [id,callback]=callbacks.entries().next().value;callbacks.delete(id);callback(now,{rtpTimestamp:rtp});}
 async function advance(ms) {
  const end = now + ms;
  while (true) {
   await settle();
   const next = [...timers].filter(([,timer])=>timer.at <= end).sort((a,b)=>a[1].at - b[1].at)[0];
   if (!next) break;
   const [id, timer] = next; now = timer.at;
   if (timer.interval) timer.at += timer.delay; else timers.delete(id);
   timer.callback();
  }
  now = end; await settle();
 }
 async function click(id) {element(id).focus();if(!element(id).disabled)return element(id).onclick?.();}
 const routePointer=(target,type,properties={})=>fire(captures.get(properties.pointerId||1)||target,type,properties);
 return {app, calls, channels, peers, timers, env, element, clipboardWrites, message, ready, advance, settle, fire, click, routePointer, captures, present, document:context.document, callbacks};
}
const tests = [];
function test(name, run) {tests.push({name, run});}
function offers(h) {return h.calls.filter(call=>call.path === '/api/offer');}

test('toolbar clipboard click works after video focus blur releases held keys', async()=>{
 const h=harness();await h.app.connect();h.ready();h.element('video').focus();
 h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});h.channels[0].sent.length=0;
 const copy=h.click('copy');await h.settle();
 assert.deepEqual(inputMessages(h),[{type:'release',revision:'0'}]);
 const request=h.channels[0].sent.find(m=>m.type==='clipboard_read');assert.ok(request,'toolbar click must request Linux clipboard');
 h.message({type:'clipboard',request:request.request,ok:true,text:'clipboard π'});await copy;
 assert.deepEqual(h.clipboardWrites,['clipboard π']);assert.equal(h.app.state.ready,false);
 await h.app.disconnect();
});
test('consumed rearm up outside video cannot swallow the next normal button release', async()=>{
 const h=harness();await h.app.connect();h.ready();h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});resetInput(h);h.channels[0].sent.length=0;
 h.routePointer('video','pointerdown');h.routePointer('outside','pointerup',{clientX:2100});
 assert.equal(h.app.state.rearmPointers.size,0);assert.equal(h.captures.size,0);assert.equal(inputMessages(h).length,0);
 h.routePointer('video','pointerdown');h.routePointer('video','pointerup');
 assert.deepEqual(inputMessages(h).map(m=>[m.type,m.down]),[['button',true],['button',false]]);assert.equal(h.app.state.buttons.size,0);
 await h.app.disconnect();
});

for(const terminal of ['pointercancel','lostpointercapture']) test(`consumed rearm ${terminal} retires capture without a remote release`,async()=>{
 const h=harness();await h.app.connect();h.ready();h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});resetInput(h);h.channels[0].sent.length=0;
 h.routePointer('video','pointerdown');h.fire('video',terminal);
 assert.equal(h.app.state.rearmPointers.size,0);assert.equal(h.captures.size,0);assert.equal(inputMessages(h).length,0);
 h.routePointer('video','pointerdown');h.routePointer('video','pointerup');
 assert.deepEqual(inputMessages(h).map(m=>[m.type,m.down]),[['button',true],['button',false]]);await h.app.disconnect();
});
for(const terminal of ['pointercancel','lostpointercapture']) test(`normal gesture ${terminal} sends one urgent release`,async()=>{
 const h=harness();await h.app.connect();h.ready();h.channels[0].sent.length=0;
 h.routePointer('video','pointerdown');h.fire('video',terminal);
 assert.deepEqual(inputMessages(h).map(m=>[m.type,m.down]),[['button',true],['release',undefined]]);
 assert.equal(h.app.state.buttons.size,0);assert.equal(h.captures.size,0);await h.app.disconnect();
});
test('failed pointer capture neither consumes rearm nor emits a native button',async()=>{
 const h=harness();await h.app.connect();h.ready();h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});resetInput(h);h.channels[0].sent.length=0;
 h.env.captureFails=true;h.routePointer('video','pointerdown');
 assert.equal(h.app.state.blocked.has('ShiftLeft'),true);assert.equal(h.app.state.rearmPointers.size,0);assert.equal(inputMessages(h).length,0);
 h.fire('video','keyup',{code:'ShiftLeft'});h.routePointer('video','pointerdown');assert.equal(inputMessages(h).length,0);await h.app.disconnect();
});
test('explicit clipboard capability permits paste across an input revision reset',async()=>{
 const h=harness(),permission=deferred();await h.app.connect();h.ready();h.element('video').focus();h.env.readText=()=>permission.promise;
 const pending=h.click('paste');await h.settle();assert.equal(h.app.state.ready,false);
 resetInput(h);permission.resolve('paste π');await h.settle();
 const request=h.channels[0].sent.find(m=>m.type==='clipboard_write');assert.equal(request.text,'paste π');
 h.message({type:'clipboard',request:request.request,ok:true});await pending;await h.app.disconnect();
});
for(const invalid of ['generation','locked','unknown','stale']) test(`deferred clipboard permission rejects ${invalid} desktop`,async()=>{
 const h=harness(),permission=deferred();await h.app.connect();h.ready();h.env.readText=()=>permission.promise;
 const pending=h.element('paste').onclick();
 if(invalid==='generation') {
  h.message({type:'frame',epoch:h.app.state.epoch,generation:2,rtp:43});
  h.present(43);
 } else h.message({type:'state',input_revision:'0',generation:1,ready:false,clipboard_ready:invalid!=='stale',lock:invalid==='stale'?'unlocked':invalid});
 permission.resolve('obsolete π');await pending;
 assert.equal(h.channels[0].sent.filter(m=>m.type==='clipboard_write').length,0);await h.app.disconnect();
});
test('Linux clipboard reply after presented geometry change cannot write browser clipboard',async()=>{
 const h=harness();await h.app.connect();h.ready();const pending=h.element('copy').onclick();
 const request=h.channels[0].sent.find(m=>m.type==='clipboard_read');
 h.message({type:'frame',epoch:h.app.state.epoch,generation:2,rtp:43});h.present(43);
 h.message({type:'clipboard',request:request.request,ok:true,text:'obsolete Linux text'});await pending;
 assert.deepEqual(h.clipboardWrites,[]);await h.app.disconnect();
});
test('clipboard state requires an explicit capability and matching presented generation',async()=>{
 const h=harness();await h.app.connect();h.ready();
 h.message({type:'state',input_revision:'0',generation:2,ready:true,clipboard_ready:true,lock:'unlocked'});
 assert.equal(h.element('copy').disabled,true);assert.equal(h.element('paste').disabled,true);
 h.message({type:'state',input_revision:'0',generation:1,ready:true,lock:'unlocked'});
 assert.equal(h.app.state.clipboardReady,false);await h.app.disconnect();
});

test('Command shortcut ends with literal ordered keyups without urgent cancellation', async()=>{
 const h=harness(); await h.app.connect(); h.ready(); h.channels[0].sent.length=0;
 h.fire('video','keydown',{code:'MetaLeft',metaKey:true});
 h.fire('video','keydown',{code:'KeyC',metaKey:true});
 h.fire('video','keyup',{code:'MetaLeft'});
 assert.deepEqual(h.channels[0].sent.filter(m=>m.type==='key'||m.type==='release').map(m=>[m.type,m.code,m.down]), [['key',29,true],['key',46,true],['key',46,false],['key',29,false]]);
 await h.app.disconnect();
});

function inputMessages(h) {return h.channels[0].sent.filter(m=>['key','button','pointer','scroll','release','input_reset_ack'].includes(m.type));}
function resetInput(h,revision='1') {
 h.message({type:'input_reset',revision});
 h.message({type:'state',input_revision:revision,ready:true,clipboard_ready:true,generation:1,lock:'unlocked',bitrate_kbps:3000,fps:30});
}
test('normal child keyup and Meta keyup preserve one Ctrl chord', async()=>{
 const h=harness();await h.app.connect();h.ready();h.channels[0].sent.length=0;
 h.fire('video','keydown',{code:'MetaLeft',metaKey:true});h.fire('video','keydown',{code:'KeyC',metaKey:true});
 h.fire('video','keyup',{code:'KeyC',metaKey:true});h.fire('video','keyup',{code:'MetaLeft'});
 assert.deepEqual(inputMessages(h).map(m=>[m.type,m.code,m.down]),[['key',29,true],['key',46,true],['key',46,false],['key',29,false]]);
 await h.app.disconnect();
});
test('physical Control and mapped Command aliases retain Ctrl until both keyups', async()=>{
 const h=harness();await h.app.connect();h.ready();h.channels[0].sent.length=0;
 h.fire('video','keydown',{code:'ControlLeft',ctrlKey:true});h.fire('video','keydown',{code:'MetaLeft',ctrlKey:true,metaKey:true});
 h.fire('video','keyup',{code:'MetaLeft',ctrlKey:true});
 assert.deepEqual(inputMessages(h).map(m=>[m.code,m.down]),[[29,true]]);
 h.fire('video','keyup',{code:'ControlLeft'});
 assert.deepEqual(inputMessages(h).map(m=>[m.code,m.down]),[[29,true],[29,false]]);
 await h.app.disconnect();
});
test('two Command sides release their distinct mapped Ctrl sides independently', async()=>{
 const h=harness();await h.app.connect();h.ready();h.channels[0].sent.length=0;
 h.fire('video','keydown',{code:'MetaLeft',metaKey:true});h.fire('video','keydown',{code:'MetaRight',metaKey:true});
 h.fire('video','keyup',{code:'MetaLeft',metaKey:true});h.fire('video','keyup',{code:'MetaRight'});
 assert.deepEqual(inputMessages(h).map(m=>[m.code,m.down]),[[29,true],[97,true],[29,false],[97,false]]);
 await h.app.disconnect();
});
test('input reset keeps video alive and blocks a held Shift chord until release and repress', async()=>{
 const h=harness();await h.app.connect();h.ready();const peer=h.app.state.pc,stream=h.element('video').srcObject;
 h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});h.channels[0].sent.length=0;resetInput(h);
 h.fire('video','keydown',{code:'KeyA',shiftKey:true});
 assert.deepEqual(inputMessages(h).map(m=>m.type),['input_reset_ack']);
 assert.equal(h.app.state.pc,peer);assert.equal(h.element('video').srcObject,stream);assert.equal(offers(h).length,1);
 h.fire('video','keyup',{code:'ShiftLeft'});h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});h.fire('video','keydown',{code:'KeyA',shiftKey:true});
 assert.deepEqual(inputMessages(h).filter(m=>m.type==='key').map(m=>[m.code,m.down,m.revision]),[[42,true,'1'],[30,true,'1']]);
 await h.app.disconnect();
});
test('stale reset and state cannot invalidate a newer held chord', async()=>{
 const h=harness();await h.app.connect();h.ready();resetInput(h,'2');h.channels[0].sent.length=0;
 h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});h.message({type:'input_reset',revision:'1'});
 h.message({type:'state',input_revision:'1',ready:false,lock:'unlocked'});h.message({type:'input_reset',revision:'2'});
 assert.equal(h.app.state.ready,true);assert.equal(h.app.state.keys.get('ShiftLeft'),42);
 assert.deepEqual(inputMessages(h).map(m=>m.type),['key']);await h.app.disconnect();
});
test('fresh modifier-free rearm click is consumed and a subsequent click reaches Linux', async()=>{
 const h=harness();await h.app.connect();h.ready();h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});resetInput(h);h.channels[0].sent.length=0;
 h.fire('video','pointerdown',{shiftKey:true});h.fire('video','pointerup',{shiftKey:true});assert.equal(inputMessages(h).length,0);
 h.fire('video','pointerdown');h.fire('video','pointerup');assert.equal(inputMessages(h).length,0);
 h.fire('video','pointerdown');h.fire('video','pointerup');assert.deepEqual(inputMessages(h).map(m=>[m.type,m.down,m.revision]),[['button',true,'1'],['button',false,'1']]);
 await h.app.disconnect();
});
test('unseen held modifiers block keyboard pointer and wheel presses', async()=>{
 const h=harness();await h.app.connect();h.ready();h.channels[0].sent.length=0;
 h.fire('video','keydown',{code:'KeyA',shiftKey:true});h.fire('video','pointerdown',{ctrlKey:true});h.fire('video','wheel',{metaKey:true,deltaX:0,deltaY:20,deltaMode:0});
 assert.equal(inputMessages(h).length,0);await h.app.disconnect();
});
test('scheduled pointer from before a reset never reaches the new revision', async()=>{
 const h=harness();await h.app.connect();h.ready();h.channels[0].sent.length=0;h.fire('video','pointermove');resetInput(h);
 await h.advance(16);assert.deepEqual(inputMessages(h).map(m=>m.type),['input_reset_ack']);
 h.fire('video','pointermove');await h.advance(16);assert.deepEqual(inputMessages(h).map(m=>m.type),['input_reset_ack','pointer']);
 assert.equal(inputMessages(h).at(-1).revision,'1');await h.app.disconnect();
});
test('blur sends one urgent revision cancellation and does not replay held keys after reset', async()=>{
 const h=harness();await h.app.connect();h.ready();h.fire('video','keydown',{code:'ControlLeft',ctrlKey:true});h.channels[0].sent.length=0;
 h.fire('window','blur');assert.equal(h.app.state.ready,false);assert.deepEqual(inputMessages(h),[{type:'release',revision:'0'}]);
 resetInput(h);h.fire('video','keydown',{code:'KeyA',ctrlKey:true});assert.deepEqual(inputMessages(h).map(m=>m.type),['release','input_reset_ack']);
 await h.app.disconnect();
});

test('reset revokes the old view and reconnects with an ordinary offer', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 const old = h.app.state.pc;
 h.message({type:'reset', reason:'Monitor geometry changed'});
 assert.equal(h.app.state.ready, false);
 assert.equal(h.element('video').srcObject, null);
 assert.equal(old.connectionState, 'closed');
 await h.advance(500);
 assert.equal(offers(h).length, 2);
 assert.equal(offers(h)[1].body.takeover, false);
 assert.notEqual(h.app.state.pc, old);
 assert.equal(h.peers[1].receiver.jitterBufferTarget, 0);
 h.ready(); assert.equal(h.app.state.recoveryStarted, null);
 await h.app.disconnect();
});

test('lost reset recovers from channel loss without Connect', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 h.app.state.channel.close(); await h.advance(500);
 assert.equal(offers(h).length, 2);
 assert.equal(offers(h)[1].body.takeover, false);
 await h.app.disconnect();
});

function hostState(h,ready=false,revision=h.app.state.inputRevision) {
 h.message({type:'state',input_revision:revision,ready,clipboard_ready:false,generation:1,lock:'unlocked',bitrate_kbps:3000,fps:30});
}
async function freshStates(h,ms,ready=false) {
 for(let elapsed=0;elapsed<ms;elapsed+=500) {if(h.app.state.channel?.readyState==='open')hostState(h,ready);await h.advance(Math.min(500,ms-elapsed));}
}
for(const annotations of [false,true]) test(`fresh host state recovers stopped matching video with ${annotations?'unmatched':'absent'} annotations`,async()=>{
 const h=harness();await h.app.connect();h.ready();const old=h.app.state.pc,lease=h.app.state.lease;
 for(let elapsed=0;elapsed<3000;elapsed+=500) {
  hostState(h);
  if(annotations) h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:100+elapsed});
  await h.advance(500);
 }
 await h.advance(500);
 assert.deepEqual(offers(h).map(call=>[call.at,call.body.takeover]),[[0,false],[3500,false]]);
 assert.equal(old.connectionState,'closed');
 assert.deepEqual(h.calls.filter(call=>call.path==='/api/stop').map(call=>call.body.lease),[lease]);
 h.ready();assert.equal(h.app.state.ready,true);await h.app.disconnect();
});

test('remote-description callbacks record the first matching presentation before establishment returns',async()=>{
 const h=harness();h.env.onRemoteDescription=()=>h.ready();await h.app.connect();
 assert.equal(h.app.state.generation,1);assert.equal(h.app.state.ready,true);
 await freshStates(h,12000,true);assert.deepEqual(offers(h).map(call=>call.at),[0]);await h.app.disconnect();
});
test('no first matching presentation retries every attempt within the original sixty-second budget',async()=>{
 const h=harness();await h.app.connect();
 for(let elapsed=0;elapsed<60000;elapsed+=500) {
  if(h.app.state.channel?.readyState==='open') {
   h.message({type:'hello',epoch:h.app.state.epoch,input_revision:'0'});hostState(h,true);
   h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:100+elapsed});
   h.present(101+elapsed);
  }
  await h.advance(500);
 }
 assert.deepEqual(offers(h).map(call=>[call.at,call.body.takeover]),[[0,false],[10500,false],[21500,false],[33500,false],[45500,false],[57500,false]]);
 assert.equal(h.app.state.intent,false);assert.equal(h.element('notice').textContent,'Recovery timed out. Use Connect to try again.');
 assert.equal(h.timers.size,0);assert.equal(h.app.state.mediaProgress,null);
 await h.advance(10000);assert.equal(offers(h).length,6);
});
test('healthy matching video does not restart during sustained false host readiness',async()=>{
 const h=harness();await h.app.connect();h.ready();
 for(let elapsed=0;elapsed<12000;elapsed+=500) {
  hostState(h);h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:100+elapsed});h.present(100+elapsed);await h.advance(500);
 }
 assert.deepEqual(offers(h).map(call=>call.at),[0]);assert.equal(h.app.state.ready,false);
 assert.equal(h.channels[0].sent.filter(m=>m.type==='presented').length,25);await h.app.disconnect();
});
test('static host-ready desktop remains usable without fresh presentation callbacks',async()=>{
 const h=harness();await h.app.connect();h.ready();await freshStates(h,12000,true);
 assert.deepEqual(offers(h).map(call=>call.at),[0]);
 h.fire('video','keydown',{code:'KeyA'});h.fire('video','keyup',{code:'KeyA'});
 assert.deepEqual(inputMessages(h).filter(m=>m.type==='key').map(m=>[m.code,m.down]),[[30,true],[30,false]]);await h.app.disconnect();
});
test('a resumed hidden session receives a full foreground grace and duplicate visibility does not renew it',async()=>{
 const h=harness();await h.app.connect();h.ready();h.document.hidden=true;h.fire('document','visibilitychange');
 await freshStates(h,12000);assert.deepEqual(offers(h).map(call=>call.at),[0]);
 h.document.hidden=false;h.fire('document','visibilitychange');await freshStates(h,2500);
 assert.deepEqual(offers(h).map(call=>call.at),[0]);h.fire('document','visibilitychange');
 await freshStates(h,1000);assert.deepEqual(offers(h).map(call=>call.at),[0,15500]);await h.app.disconnect();
});
test('initially hidden first-frame grace begins on visibility while the recovery budget keeps its original start',async()=>{
 const h=harness();h.document.hidden=true;await h.app.connect();
 h.message({type:'hello',epoch:h.app.state.epoch,input_revision:'0'});await freshStates(h,55000);
 assert.deepEqual(offers(h).map(call=>call.at),[0]);assert.equal(h.app.state.recoveryStarted,0);
 h.document.hidden=false;h.fire('document','visibilitychange');await freshStates(h,5000);
 assert.equal(h.app.state.intent,false);assert.equal(h.element('notice').textContent,'Recovery timed out. Use Connect to try again.');assert.equal(h.timers.size,0);
});
test('Disconnect during resumed media grace cancels all retries and releases the old lease',async()=>{
 const h=harness();await h.app.connect();h.ready();const lease=h.app.state.lease;
 h.document.hidden=true;h.fire('document','visibilitychange');await freshStates(h,12000);
 h.document.hidden=false;h.fire('document','visibilitychange');await freshStates(h,1000);await h.app.disconnect();await h.advance(12000);
 assert.deepEqual(offers(h).map(call=>call.at),[0]);assert.deepEqual(h.calls.filter(c=>c.path==='/api/stop').map(c=>c.body.lease),[lease]);assert.equal(h.timers.size,0);
});
test('a matching ready report clears the sustained pause and a later false report starts a new interval',async()=>{
 const h=harness();await h.app.connect();h.ready();await freshStates(h,2500);await freshStates(h,1000,true);
 await freshStates(h,2500);assert.deepEqual(offers(h).map(c=>c.at),[0]);
 await freshStates(h,1000);assert.deepEqual(offers(h).map(c=>c.at),[0,7000]);await h.app.disconnect();
});
test('obsolete input revisions cannot start or clear the media pause',async()=>{
 const h=harness();await h.app.connect();h.ready();
 for(let elapsed=0;elapsed<5000;elapsed+=500) {hostState(h,true);hostState(h,false,'obsolete');await h.advance(500);}
 assert.deepEqual(offers(h).map(c=>c.at),[0]);
 hostState(h);await freshStates(h,2500);hostState(h,true,'obsolete');await freshStates(h,1000);
 assert.deepEqual(offers(h).map(c=>c.at),[0,8500]);await h.app.disconnect();
});
test('unmatched callbacks and obsolete epoch annotations do not refresh matching presentation progress',async()=>{
 const h=harness();await h.app.connect();h.ready();
 for(let elapsed=0;elapsed<3000;elapsed+=500) {
  hostState(h);h.message({type:'frame',epoch:'obsolete',generation:1,rtp:100+elapsed});h.present(100+elapsed);await h.advance(500);
 }
 await h.advance(500);assert.deepEqual(offers(h).map(c=>c.at),[0,3500]);await h.app.disconnect();
});
test('a late frame annotation uses its original callback time rather than its arrival time',async()=>{
 const h=harness();await h.app.connect();h.ready();await freshStates(h,1000);h.present(43);
 await freshStates(h,1500);h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:43});
 assert.deepEqual(h.channels[0].sent.filter(m=>m.type==='presented').map(m=>m.rtp),[42,43]);
 await freshStates(h,1000);assert.deepEqual(offers(h).map(c=>c.at),[0]);
 await freshStates(h,1000);assert.deepEqual(offers(h).map(c=>c.at),[0,4500]);await h.app.disconnect();
});
test('late annotations cannot move matching presentation time backwards',async()=>{
 const h=harness();await h.app.connect();h.ready();await freshStates(h,500);h.present(43);
 await freshStates(h,1000);h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:44});h.present(44);
 await freshStates(h,1000);h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:43});
 await freshStates(h,1500);assert.deepEqual(offers(h).map(c=>c.at),[0]);
 await freshStates(h,1000);assert.deepEqual(offers(h).map(c=>c.at),[0,5000]);await h.app.disconnect();
});
test('recovery releases held Shift and keeps it blocked without replay until its physical release',async()=>{
 const h=harness();await h.app.connect();h.ready();h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});
 await freshStates(h,3000);await h.advance(500);h.ready();
 assert.deepEqual(h.channels[0].sent.filter(m=>m.type==='key'||m.type==='release'),[{type:'key',revision:'0',generation:1,code:42,down:true},{type:'release',revision:'0'}]);
 h.fire('video','keydown',{code:'KeyA',shiftKey:true});h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true,repeat:true});
 assert.equal(h.channels[1].sent.filter(m=>m.type==='key').length,0);assert.equal(h.app.state.blocked.has('ShiftLeft'),true);
 h.fire('video','keyup',{code:'ShiftLeft'});h.fire('video','keydown',{code:'KeyA'});h.fire('video','keyup',{code:'KeyA'});
 assert.deepEqual(h.channels[1].sent.filter(m=>m.type==='key').map(m=>[m.code,m.down]),[[30,true],[30,false]]);await h.app.disconnect();
});
test('obsolete session callbacks states and watchdogs cannot change the replacement session',async()=>{
 const h=harness();await h.app.connect();h.ready();const oldChannel=h.channels[0],oldCallback=[...h.callbacks.values()][0];
 const oldWatchdog=h.timers.get(h.app.state.watchdog).callback;
 await h.app.connect();h.ready();await freshStates(h,2500,true);
 oldChannel.onmessage({data:JSON.stringify({type:'state',input_revision:'0',ready:false})});oldCallback(2500,{rtpTimestamp:43});
 oldWatchdog();assert.equal(h.app.state.ready,true);await freshStates(h,12000,true);
 assert.deepEqual(offers(h).map(c=>c.at),[0,0]);await h.app.disconnect();
});

test('lost channel messages recover through the three-second watchdog', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 await h.advance(3000);
 assert.equal(h.app.state.ready, false);
 assert.equal(h.element('video').srcObject, null);
 await h.advance(500);
 assert.equal(offers(h).length, 2);
 await h.app.disconnect();
});

test('manual Disconnect cancels a queued reconnect', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 h.message({type:'reset', reason:'Desktop lock state changed'});
 await h.app.disconnect(); await h.advance(5000);
 assert.equal(offers(h).length, 1);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.timers.size, 0);
});

test('a late accepted offer stops only its own lease', async()=>{
 const h = harness(), delayed = deferred();
 const original = h.env.fetch; let first = true;
 h.env.fetch = call => call.path === '/api/offer' && first ? (first = false, delayed.promise) : original(call);
 const firstConnect = h.app.connect(); await h.settle();
 assert.equal(offers(h).length, 1);
 await h.app.disconnect();
 const secondConnect = h.app.connect(); await h.settle();
 assert.equal(offers(h).length, 1);
 delayed.resolve({status:200, ok:true, json:async()=>({sdp:'answer', lease:'a'.repeat(64), epoch:'old'})});
 await firstConnect; await secondConnect;
 assert.equal(offers(h).length, 2);
 assert.deepEqual(h.calls.filter(call=>call.path === '/api/stop').map(call=>call.body.lease), ['a'.repeat(64)]);
 assert.notEqual(h.app.state.lease, 'a'.repeat(64));
 await h.app.disconnect();
});

for (const status of [400, 403, 409]) test(`explicit HTTP ${status} stops connection intent`, async()=>{
 const h = harness();
 h.env.fetch = async()=>({status, ok:false, json:async()=>({error:'terminal admission error'})});
 await h.app.connect(); await h.advance(10000);
 assert.equal(offers(h).length, 1);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.element('notice').textContent, 'terminal admission error');
 assert.equal(h.timers.size, 0);
});

for (const status of [400, 403]) test(`automatic HTTP ${status} stops recovery`, async()=>{
 const h = harness();
 h.env.fetch = async call => ({status:call.at === 0 ? 503 : status, ok:false, json:async()=>({error:'terminal admission error'})});
 await h.app.connect(); await h.advance(10000);
 assert.deepEqual(offers(h).map(call=>call.at), [0, 500]);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.element('notice').textContent, 'terminal admission error');
 assert.equal(h.timers.size, 0);
});

test('a lost accepted answer retries conflicts until the unknown lease expires', async()=>{
 const h = harness(), original = h.env.fetch;
 h.env.fetch = call => {
  if (call.path !== '/api/offer') return original(call);
  if (call.at === 0) return Promise.reject(new Error('Accepted answer lost'));
  if (call.at < 15000) return Promise.resolve({status:409, ok:false, json:async()=>({error:'Controller active'})});
  return original(call);
 };
 await h.app.connect(true); await h.advance(14500);
 assert.equal(h.app.state.intent, true);
 assert.equal(h.app.state.lease, null);
 assert.deepEqual(offers(h).map(call=>call.at), [0, 500, 1500, 3500, 5500, 7500, 9500, 11500, 13500]);
 await h.advance(1000); h.ready();
 assert.equal(h.app.state.lease, '1'.padStart(64, '0'));
 assert.equal(h.app.state.recoveryStarted, null);
 assert.deepEqual(offers(h).map(call=>call.body.takeover), [true, false, false, false, false, false, false, false, false, false]);
 await h.app.disconnect();
});

test('a lost Stop retries conflicts until the old controller reaches its deadman', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 const oldLease = h.app.state.lease, original = h.env.fetch;
 h.env.fetch = call => {
  if (call.path === '/api/stop') return new Promise((_,reject)=>call.signal.addEventListener('abort',()=>reject(new Error('Stop lost'))));
  if (call.at < 15000) return Promise.resolve({status:409, ok:false, json:async()=>({error:'Old controller active'})});
  return original(call);
 };
 h.message({type:'reset', reason:'Desktop changed'});
 await h.advance(14500);
 assert.equal(h.app.state.intent, true);
 assert.equal(h.app.state.lease, null);
 assert.deepEqual(h.calls.filter(call=>call.path === '/api/stop').map(call=>call.body.lease), [oldLease]);
 await h.advance(1000); h.ready();
 assert.equal(h.app.state.lease, '2'.padStart(64, '0'));
 assert.equal(h.app.state.recoveryStarted, null);
 assert.ok(offers(h).every(call=>call.body.takeover === false));
 const stopped = h.app.disconnect(); await h.advance(1000); await stopped;
 assert.equal(h.timers.size, 0);
});

test('another controller can hold automatic conflicts until the recovery budget expires', async()=>{
 const h = harness();
 h.env.fetch = async call => ({status:call.at === 0 ? 503 : 409, ok:false, json:async()=>({error:'Controller active'})});
 await h.app.connect(); await h.advance(59500);
 assert.equal(h.app.state.intent, true);
 assert.equal(h.app.state.recoveryStarted, 0);
 assert.equal(offers(h).at(-1).at, 59500);
 await h.advance(500);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.element('notice').textContent, 'Recovery timed out. Use Connect to try again.');
 assert.equal(h.timers.size, 0);
 const attempts = offers(h).length; await h.advance(10000);
 assert.equal(offers(h).length, attempts);
 assert.ok(offers(h).every(call=>call.body.takeover === false));
});

test('Disconnect cancels automatic conflict recovery', async()=>{
 const h = harness();
 h.env.fetch = async call => ({status:call.at === 0 ? 503 : 409, ok:false, json:async()=>({error:'Controller active'})});
 await h.app.connect(); await h.advance(3500);
 assert.deepEqual(offers(h).map(call=>call.at), [0, 500, 1500, 3500]);
 await h.app.disconnect(); await h.advance(60000);
 assert.equal(offers(h).length, 4);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.timers.size, 0);
});

test('an offer started near the recovery deadline cannot outlive the budget and releases a late lease', async()=>{
 const h = harness(), delayed = deferred();
 h.env.fetch = call => {
  if (call.path === '/api/stop') return Promise.resolve({status:204, ok:true});
  if (call.at > 58000) return delayed.promise;
  return Promise.resolve({status:call.at === 0 ? 503 : 409, ok:false, json:async()=>({error:'Controller active'})});
 };
 await h.app.connect(); await h.advance(59500);
 assert.equal(h.app.state.intent, true);
 assert.equal(h.app.state.connecting, true);
 await h.advance(500);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.app.state.pc, null);
 assert.equal(h.element('notice').textContent, 'Recovery timed out. Use Connect to try again.');
 delayed.resolve({status:200, ok:true, json:async()=>({sdp:'answer', lease:'c'.repeat(64), epoch:'late'})});
 await h.settle();
 assert.deepEqual(h.calls.filter(call=>call.path === '/api/stop').map(call=>call.body.lease), ['c'.repeat(64)]);
 assert.equal(h.timers.size, 0);
});

test('compatible H264 codec preferences retain RTX and exclude other video codecs', async()=>{
 const h = harness();
 h.env.codecs = [
  {mimeType:'video/VP8'},
  {mimeType:'video/H264', sdpFmtpLine:'packetization-mode=1;profile-level-id=640c1f'},
  {mimeType:'video/H264', sdpFmtpLine:'packetization-mode=0;profile-level-id=42e01f'},
  {mimeType:'video/H264', sdpFmtpLine:'packetization-mode=1;profile-level-id=42e01f'},
  {mimeType:'video/rtx'},
  {mimeType:'video/RTX'},
 ];
 await h.app.connect();
 assert.deepEqual(h.peers[0].codecPreferences, [
  {mimeType:'video/H264', sdpFmtpLine:'packetization-mode=1;profile-level-id=42e01f'},
  {mimeType:'video/rtx'},
  {mimeType:'video/RTX'},
 ]);
 assert.equal(offers(h).length, 1);
 await h.app.disconnect();
});

for (const codecs of [
 [{mimeType:'video/VP8'}, {mimeType:'video/rtx'}],
 [{mimeType:'video/H264', sdpFmtpLine:'packetization-mode=1;profile-level-id=640c1f'}, {mimeType:'video/rtx'}],
]) test(`RTX without compatible H264 rejects ${codecs[0].sdpFmtpLine ? 'the wrong profile' : 'a browser without H264'}`, async()=>{
 const h = harness(); h.env.codecs = codecs;
 await h.app.connect(); await h.advance(10000);
 assert.equal(offers(h).length, 0);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.element('notice').textContent, 'This browser has no compatible H.264 desktop decoder');
 assert.equal(h.timers.size, 0);
});

test('HTTP 503 retries with bounded backoff and stops after sixty seconds', async()=>{
 const h = harness();
 h.env.fetch = async()=>({status:503, ok:false, json:async()=>({error:'temporary host failure'})});
 await h.app.connect(); assert.equal(offers(h).length, 1);
 await h.advance(500); assert.equal(offers(h).length, 2);
 await h.advance(1000); assert.equal(offers(h).length, 3);
 await h.advance(2000); assert.equal(offers(h).length, 4);
 await h.advance(56500);
 assert.equal(h.app.state.intent, false);
 assert.equal(h.timers.size, 0);
 const attempts = offers(h).length; await h.advance(10000);
 assert.equal(offers(h).length, attempts);
 assert.ok(offers(h).every(call=>call.body.takeover === false));
});

test('a hung offer is bounded and manual Disconnect cancels its continuations', async()=>{
 const h = harness();
 h.env.fetch = call => new Promise((_,reject)=>call.signal.addEventListener('abort',()=>reject(new Error('aborted'))));
 const connecting = h.app.connect(); await h.settle();
 await h.app.disconnect(); await connecting;
 assert.equal(h.app.state.intent, false);
 await h.advance(10000);
 assert.equal(offers(h).length, 1);
 assert.equal(h.timers.size, 0);
 assert.equal(h.app.state.pc, null);
});

test('a late parsed answer after request timeout still releases its accepted lease', async()=>{
 const h = harness(), delayed = deferred();
 const original = h.env.fetch; let first = true;
 h.env.fetch = call => call.path === '/api/offer' && first ? (first = false, delayed.promise) : original(call);
 const connecting = h.app.connect(); await h.settle(); await h.advance(10000); await connecting;
 delayed.resolve({status:200, ok:true, json:async()=>({sdp:'answer', lease:'b'.repeat(64), epoch:'late'})});
 await h.settle();
 assert.deepEqual(h.calls.filter(call=>call.path === '/api/stop').map(call=>call.body.lease), ['b'.repeat(64)]);
 await h.app.disconnect();
});

test('a blocked Stop request cannot hold reconnect indefinitely', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 const original = h.env.fetch;
 h.env.fetch = call => call.path === '/api/stop' ? new Promise((_,reject)=>call.signal.addEventListener('abort',()=>reject(new Error('aborted')))) : original(call);
 h.message({type:'reset', reason:'Desktop changed'});
 await h.advance(500);
 assert.equal(offers(h).length, 2);
 await h.advance(500); h.ready();
 const stopped = h.app.disconnect(); await h.advance(1000); await stopped;
 assert.equal(h.timers.size, 0);
});

test('Take control is explicit and its automatic recovery cannot take over', async()=>{
 const h = harness(); await h.app.connect(true); h.ready();
 assert.equal(offers(h)[0].body.takeover, true);
 h.message({type:'reset', reason:'Desktop changed'}); await h.advance(500);
 assert.equal(offers(h)[1].body.takeover, false);
 await h.app.disconnect();
});

test('deferred browser clipboard permission cannot paste into a replacement session', async()=>{
 const h = harness(), permission = deferred(); await h.app.connect(); h.ready();
 h.env.readText = ()=>permission.promise;
 const pending = h.element('paste').onclick();
 await h.app.connect(); h.ready();
 const notice = h.element('notice').textContent;
 permission.resolve('old dummy text'); await pending;
 assert.equal(h.channels.flatMap(channel=>channel.sent).filter(message=>message.type === 'clipboard_write').length, 0);
 assert.equal(h.element('notice').textContent, notice);
 await h.app.disconnect();
});

test('Linux clipboard completion cannot start browser copy after session replacement', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 const pending = h.element('copy').onclick();
 const request = h.channels[0].sent.find(message=>message.type === 'clipboard_read').request;
 h.message({type:'clipboard', request, ok:true, text:'old dummy Linux text'});
 const replacement = h.app.connect(); await replacement; h.ready(); await pending;
 assert.deepEqual(h.clipboardWrites, []);
 await h.app.disconnect();
});

test('a deferred browser clipboard write cannot replace a newer session notice', async()=>{
 const h = harness(), permission = deferred(); await h.app.connect(); h.ready();
 h.env.writeText = text => {h.clipboardWrites.push(text); return permission.promise;};
 const pending = h.element('copy').onclick();
 const request = h.channels[0].sent.find(message=>message.type === 'clipboard_read').request;
 h.message({type:'clipboard', request, ok:true, text:'dummy Linux text'}); await h.settle();
 assert.deepEqual(h.clipboardWrites, ['dummy Linux text']);
 await h.app.connect(); h.ready(); const notice = h.element('notice').textContent;
 permission.resolve(); await pending;
 assert.equal(h.element('notice').textContent, notice);
 await h.app.disconnect();
});

test('pending Linux clipboard requests reject on disconnect and successful text is unchanged', async()=>{
 const h = harness(); await h.app.connect(); h.ready();
 const pending = h.element('paste').onclick(); await h.settle();
 const request = h.channels[0].sent.find(message=>message.type === 'clipboard_write');
 assert.equal(request.text, 'dummy browser text');
 h.message({type:'clipboard', request:request.request, ok:true}); await pending;
 assert.ok(h.element('notice').textContent.includes('copied to the Linux clipboard'));
 const read = h.element('copy').onclick(); await h.app.disconnect(); await read;
 assert.equal(h.app.state.pendingClipboard.size, 0);
 assert.equal(h.timers.size, 0);
});

test('quality change releases held input and starts a fresh session at the selected mode',async()=>{
 const h=harness();await h.app.connect();h.ready();
 assert.equal(offers(h)[0].body.quality,'clear');
 h.fire('video','keydown',{code:'ShiftLeft',shiftKey:true});
 h.element('quality').value='smooth';h.element('quality').onchange();
 assert.equal(h.app.state.ready,false);
 assert.ok(h.channels[0].sent.some(message=>message.type==='release'));
 await h.advance(500);
 assert.equal(offers(h)[1].body.quality,'smooth');
 assert.notEqual(h.channels[0],h.app.state.channel);
 h.ready();
 h.element('quality').value='low_data';h.element('quality').onchange();await h.advance(500);
 assert.equal(offers(h)[2].body.quality,'low_data');
 await h.app.disconnect();
});

test('a stalled smooth stream reconnects in clear mode',async()=>{
 const h=harness();h.element('quality').value='smooth';h.element('quality').onchange();
 await h.app.connect();h.ready();assert.equal(offers(h)[0].body.quality,'smooth');
 h.app.state.mediaProgress.pausedSince=0;h.app.state.mediaProgress.lastMatchedAt=0;
 await h.advance(2500);
 assert.equal(h.app.state.quality,'clear');assert.equal(h.element('quality').value,'clear');
 await h.advance(500);
 assert.equal(offers(h)[1].body.quality,'clear');
 await h.app.disconnect();
});

test('smooth mode falls back when the host stays stale despite new browser frames',async()=>{
 const h=harness();h.element('quality').value='smooth';h.element('quality').onchange();
 await h.app.connect();h.ready();
 const state=()=>h.message({type:'state',input_revision:h.app.state.inputRevision,ready:false,clipboard_ready:false,generation:1,lock:'unlocked',bitrate_kbps:2400,fps:20});
 state();await h.advance(2000);
 h.message({type:'frame',epoch:h.app.state.epoch,generation:1,rtp:43});h.present(43);state();
 await h.advance(500);
 assert.equal(h.app.state.quality,'clear');
 await h.advance(500);
 assert.equal(offers(h)[1].body.quality,'clear');
 await h.app.disconnect();
});

(async()=>{
 const selected=tests.filter(({name})=>!process.env.TWA_TEST_FILTER||name.includes(process.env.TWA_TEST_FILTER));
 assert.ok(selected.length,'test filter must select a behavioral check');
 for (const {name, run} of selected) {await run(); console.log(`PASS ${name}`);}
 console.log(`${selected.length} client lifecycle tests passed`);
})().catch(error=>{console.error(error); process.exitCode = 1;});
