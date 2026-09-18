const {test}=require('node:test');
const assert=require('node:assert/strict');
const cache=require('../cache-metrics.js');
const record=(input=1000,cached=900,extra={})=>({kind:'responses',state:'completed',account_id:'account-a',binding_id:'binding-a',binding_generation:1,provider:'openai',model:'gpt',created_at:'2026-09-11T02:00:02Z',finished_at:'2026-09-11T02:00:03Z',usage:{complete:true,input_tokens:input,cached_input_tokens:cached},...extra});
const previous=()=>record(1000,900,{id:'previous',created_at:'2026-09-11T02:00:00Z',finished_at:'2026-09-11T02:00:01Z'});
test('uses total input as denominator and preserves a reported zero hit rate',()=>{
 assert.equal(cache.measure(record()).rate,90);
 assert.equal(cache.measure(record(1000,0)).rate,0);
 assert.equal(cache.measure(record(1000,1000)).rate,100);
});
test('unknown, incomplete, zero-input, invalid and Search usage never become zero-percent hits',()=>{
 for(const r of [record(null,0),record(1000,null),record(0,0),record(1000,1001),record(-1,0),record(1000,-1),record(1000,.5),record(1000,0,{usage:{complete:false,input_tokens:1000,cached_input_tokens:0}}),record(1000,0,{kind:'search'})])assert.equal(cache.measure(r).rate,null);
});
test('the weighted summary is not an average of percentages and excludes Search or unknown usage',()=>{
 const s=cache.summarize([record(1000,900),record(100,0),record(null,null),record(5000,0,{kind:'search'})]);
 assert.equal(s.rate,900/1100*100);assert.equal(s.minimum,0);assert.equal(s.measured,2);assert.equal(s.unknown,1);
});
test('drop threshold uses percentage points, including its boundary',()=>{
 assert.equal(cache.compare(record(1000,700,{cache_previous:previous()})).drop,true);
 assert.equal(cache.compare(record(1000,701,{cache_previous:previous()})).drop,false);
 assert.equal(cache.compare(record(1000,453,{cache_previous:{...previous(),usage:{complete:true,input_tokens:1000,cached_input_tokens:653}}})).drop,true);
 assert.equal(cache.compare(record(1000,990,{cache_previous:previous()})).delta,9);
});
test('first recorded request and unavailable previous usage are not drop alerts',()=>{
 assert.equal(cache.compare(record(1000,0)).drop,false);
 assert.equal(cache.compare(record(1000,0,{cache_previous:record(null,null)})).delta,null);
});
test('account, binding and model changes are separate context markers',()=>{
 for(const extra of [{account_id:'account-b'},{binding_id:'binding-b'},{binding_generation:2},{model:'other'},{provider:'other'}]){
  const m=cache.compare(record(1000,0,{...extra,cache_previous:previous()}));assert.equal(m.drop,false);assert.equal(m.delta,null);assert.ok(m.changes.length);
 }
 // An alias change that resolves to the same upstream model is comparable.
 assert.equal(cache.compare(record(1000,0,{model:'alias-b',upstream_model:'same',cache_previous:{...previous(),model:'alias-a',upstream_model:'same'}})).drop,true);
});
test('overlapping or unfinished predecessors are not warm-cache baselines',()=>{
 for(const finished_at of [null,'2026-09-11T02:00:04Z'])assert.equal(cache.compare(record(1000,0,{cache_previous:{...previous(),finished_at}})).delta,null);
});

test('stateless requests retain measured hits without claiming session continuity',()=>{
 const r=record(1000,400,{stateless:true,cache_previous:previous()});
 assert.equal(cache.measure(r).rate,40);assert.equal(cache.compare(r).delta,null);assert.equal(cache.compare(r).drop,false);
 assert.equal(cache.summarize([r]).rate,40);
});
