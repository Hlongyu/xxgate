const test=require('node:test'),assert=require('node:assert/strict');
const runtime=require('../runtime-telemetry.js');
test('history deduplicates cursor overlaps, sorts and retains only five minutes',()=>{
 const a=Array.from({length:400},(_,i)=>({at_ms:i*1000,queued:0}));
 const history=runtime.merge(a,[{at_ms:399000,queued:5},{at_ms:400000,queued:1}]);
 assert.equal(history.length,300);assert.equal(history[0].at_ms,101000);assert.equal(history.at(-2).queued,5);
});
test('missing CPU and delayed samples create gaps instead of invented zeroes',()=>{
 const samples=[{at_ms:1000,cpu:null},{at_ms:2000,cpu:40},{at_ms:3000,cpu:null},{at_ms:4000,cpu:50},{at_ms:9000,cpu:60}];
 assert.equal((runtime.path(samples,'cpu',100).match(/M/g)||[]).length,3);
 const html=runtime.content([{at_ms:1000,cpu_capacity_percent:null,memory_bytes:null,probe_error:'<secret>',inflight:0,queued:0}]);
 assert.match(html,/采样不可用/);assert.doesNotMatch(html,/<secret>|NaN|undefined/);assert.match(html,/—/);
});
test('polling does not overlap, keeps old data on errors and stops after leaving',async()=>{
 let calls=0,resolve,enabled=true;const scheduled=[],nodes=new Map([['telemetry-content',{}],['telemetry-status',{}]]);
 const control=runtime.create({read:()=>{calls++;return new Promise(r=>resolve=r);},eligible:()=>enabled,node:id=>nodes.get(id),now:()=>3000,schedule:fn=>{scheduled.push(fn);return scheduled.length;},cancel(){}});
 control.mount();control.mount();assert.equal(calls,1);
 resolve({samples:[{at_ms:2000,cpu_capacity_percent:50,inflight:1,queued:2}]});await new Promise(r=>setImmediate(r));
 assert.match(nodes.get('telemetry-content').innerHTML,/50.0/);assert.equal(scheduled.length,1);
 enabled=false;control.stop();await scheduled[0]();assert.equal(calls,1);
});

test('switching workers resets history and the polling cursor',async()=>{
 const paths=[],nodes=new Map([['telemetry-content',{}],['telemetry-status',{}]]);
 let worker=1;
 const control=runtime.create({read:async path=>{paths.push(path);return {samples:[{at_ms:worker*1000,cpu_capacity_percent:worker*11}]};},eligible:()=>true,node:id=>nodes.get(id),now:()=>3000,schedule:()=>1,cancel(){}});
 control.mount();await new Promise(setImmediate);
 assert.match(nodes.get('telemetry-content').innerHTML,/11.0/);
 control.reset();worker=2;control.mount();await new Promise(setImmediate);
 assert.deepEqual(paths,['/telemetry','/telemetry']);
 assert.match(nodes.get('telemetry-content').innerHTML,/22.0/);
 assert.doesNotMatch(nodes.get('telemetry-content').innerHTML,/11.0/);
 control.stop();
});
