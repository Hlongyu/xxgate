const test=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const path=require('node:path');
const vm=require('node:vm');
process.env.TZ='Asia/Shanghai';

function harness(){
 const events={},timers=[],calls=[],nodes=new Map();
 function node(){return {innerHTML:'',textContent:'',open:false,querySelector(){return null;},addEventListener(){},remove(){},classList:{add(){},remove(){}}};}
 for(const id of ['#app','#dialog','#main','#breadcrumb','#updated','#request-refresh-status','#request-results','#toast'])nodes.set(id,node());
 const draft={value:'正在输入的新条件'};
 const document={hidden:false,querySelector(selector){return nodes.get(selector)||(selector.includes(':focus')?draft:null);},addEventListener(name,fn){events[name]=fn;},
  createElement(){return {innerHTML:'',get content(){const html=this.innerHTML;return {querySelector(){const marker='<div id="request-results">';return {innerHTML:html.slice(html.indexOf(marker)+marker.length,-12)};}};}};}};
 const context={document,window:{addEventListener(){}},location:{hash:'#requests'},URLSearchParams,AbortController,Date,console,
  CacheMetrics:require('../cache-metrics.js'),ErrorCenter:{page(){}},setInterval(fn,ms){timers.push({fn,ms});},setTimeout(){},clearTimeout(){},clearInterval(){},
  FormData:class{constructor(form){return Object.entries(form.values);}},fetch:()=>new Promise(()=>{})};
 vm.createContext(context);
 vm.runInContext(fs.readFileSync(path.join(__dirname,'../app.js'),'utf8')+'\nglobalThis.requestTest={state,requestFilter,requestLocalTime,requestTodayFilter,refreshCurrentPage,render};',context);
 context.fetch=async url=>{calls.push(url);return {ok:true,status:200,json:async()=>({items:[],total:calls.length})};};
 context.requestTest.state.auth=true;context.requestTest.state.page='requests';
 return {...context.requestTest,context,document,events,timers,calls,nodes,draft};
}

test('default requests cover the local calendar day and reset restores today',async()=>{
 const h=harness(),today=h.requestTodayFilter();
 assert.deepEqual(h.state.filter,today);
 const boundary=h.requestTodayFilter(new Date('2026-10-31T23:59:59+08:00'));
 assert.equal(boundary.from,'2026-10-30T16:00:00.000Z');
 assert.equal(boundary.to,'2026-10-31T16:00:00.000Z');
 await h.render(true);
 let params=new URL(h.calls[0],'http://localhost').searchParams;
 assert.equal(params.get('from'),today.from);assert.equal(params.get('to'),today.to);
 h.state.filter={model:'old',from:'2020-01-01T00:00:00Z'};h.state.offset=25;
 const button={dataset:{action:'filter-reset'}};
 await h.events.click({target:{closest:selector=>selector==='[data-billing]'?null:button}});
 params=new URL(h.calls[1],'http://localhost').searchParams;
 assert.equal(params.get('from'),today.from);assert.equal(params.get('to'),today.to);
 assert.equal(params.get('model'),null);assert.equal(params.get('offset'),'0');
 assert.deepEqual(Object.keys(h.requestFilter({from:'',to:''})),[],'clearing the range explicitly remains supported');
});

test('request time filters use local seconds and UTC API boundaries, including open-ended ranges',()=>{
 const h=harness(),filter=h.requestFilter({from:'2026-10-25T08:15:30',to:'2026-10-25T09:20:45',session_id:' session ',lookup_kind:'key_id',lookup:' key '});
 assert.equal(filter.from,'2026-10-25T00:15:30.000Z');
 assert.equal(filter.to,'2026-10-25T01:20:45.000Z');
 assert.equal(h.requestLocalTime(filter.from),'2026-10-25T08:15:30');
 assert.equal(filter.session_id,'session');assert.equal(filter.key_id,'key');
 assert.equal(h.requestFilter({from:'',to:'2026-10-25T09:20:45'}).from,undefined);
 assert.equal(h.requestFilter({from:'2026-10-25T08:15:30'}).to,undefined);
 assert.throws(()=>h.requestFilter({from:'bad'}),/有效/);
 for(const to of ['2026-10-25T07:00:00','2026-10-25T08:15:30'])assert.throws(()=>h.requestFilter({from:'2026-10-25T08:15:30',to}),/晚于/);
});

test('manual refresh updates results without replacing unsaved filters or losing pagination',async()=>{
 const h=harness();h.state.filter={model:'applied-model',from:'2026-10-25T00:00:00Z'};h.state.offset=25;
 h.nodes.get('#main').innerHTML='stable form with unsaved edits';
 let table={scrollLeft:180},html='old rows';const results=h.nodes.get('#request-results');
 results.querySelector=()=>table;
 Object.defineProperty(results,'innerHTML',{get:()=>html,set:value=>{html=value;table={scrollLeft:0};}});
 await h.render(true);
 assert.equal(h.calls.length,1);
 const params=new URL(h.calls[0],'http://localhost').searchParams;
 assert.equal(params.get('model'),'applied-model');assert.equal(params.get('from'),h.state.filter.from);assert.equal(params.get('offset'),'25');
 assert.equal(h.nodes.get('#main').innerHTML,'stable form with unsaved edits');
 assert.equal(h.draft.value,'正在输入的新条件');
 assert.equal(table.scrollLeft,180);
 assert.match(h.nodes.get('#request-results').innerHTML,/1 条匹配/);
 await h.render(true);assert.match(h.nodes.get('#request-results').innerHTML,/2 条匹配/);
 assert.match(h.nodes.get('#request-refresh-status').textContent,/更新于/);
});

test('request results never refresh on the timer or when returning to the tab',async()=>{
 const h=harness();h.nodes.get('#request-results').innerHTML='fixed query result';
 assert.equal(h.timers[0].ms,10000);
 await h.timers[0].fn();await h.timers[0].fn();
 h.document.hidden=true;h.events.visibilitychange();h.document.hidden=false;h.events.visibilitychange();
 await new Promise(setImmediate);
 assert.equal(h.calls.length,0);assert.equal(h.nodes.get('#request-results').innerHTML,'fixed query result');
 await h.render(true);assert.equal(h.calls.length,1,'explicit refresh still queries the server');
});

test('changing the cache-drop display uses the existing query snapshot without fetching new records',async()=>{
 const h=harness();await h.render(true);const updated=h.nodes.get('#request-refresh-status').textContent;
 const button={dataset:{action:'cache-drops'}};
 const event={target:{closest:selector=>selector==='[data-billing]'?null:button}};
 await h.events.click(event);assert.equal(h.state.cacheDropsOnly,true);
 await h.events.click(event);assert.equal(h.state.cacheDropsOnly,false);
 assert.equal(h.calls.length,1);assert.equal(h.nodes.get('#request-refresh-status').textContent,updated);
 button.dataset.action='refresh';await h.events.click(event);
 assert.equal(h.calls.length,2,'only the explicit refresh button fetches again');
});

test('manual refreshes do not overlap; failures retain the results until a successful retry',async()=>{
 const h=harness();let finish;
 h.context.fetch=url=>{h.calls.push(url);return new Promise(resolve=>{finish=resolve;});};
 h.nodes.get('#request-results').innerHTML='previous rows';
 const pending=h.render(true);await h.render(true);assert.equal(h.calls.length,1);
 finish({ok:false,status:503,json:async()=>({error:{message:'unavailable'}})});await pending;
 assert.equal(h.nodes.get('#request-results').innerHTML,'previous rows');
 assert.match(h.nodes.get('#request-refresh-status').textContent,/刷新失败/);
 h.context.fetch=async()=>({ok:true,status:200,json:async()=>({items:[],total:3})});
 await h.render(true);assert.match(h.nodes.get('#request-results').innerHTML,/3 条匹配/);
});

test('invalid time submission preserves the applied query; valid submission resets pagination and sends UTC',async()=>{
 const h=harness(),error={textContent:''},button={disabled:false};h.state.filter={model:'old'};h.state.offset=25;
 const form={dataset:{form:'filter'},values:{from:'2026-10-25T09:00:00',to:'2026-10-25T08:00:00'},querySelector:s=>s==='.text-error'?error:button};
 await h.events.submit({target:form,preventDefault(){}});
 assert.equal(h.calls.length,0);assert.equal(h.state.filter.model,'old');assert.equal(h.state.offset,25);assert.match(error.textContent,/晚于/);
 form.values={from:'2026-10-25T08:00:00',to:'2026-10-25T09:00:00',model:'gpt-6.1-sol'};
 await h.events.submit({target:form,preventDefault(){}});
 assert.equal(h.state.offset,0);const params=new URL(h.calls[0],'http://localhost').searchParams;
 assert.equal(params.get('from'),'2026-10-25T00:00:00.000Z');assert.equal(params.get('to'),'2026-10-25T01:00:00.000Z');assert.equal(params.get('model'),'gpt-6.1-sol');assert.equal(button.disabled,false);
});
