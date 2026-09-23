const test=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

test('content policy rejection renders its category and specific cause for HTTP 200 streams',async()=>{
 const context={document:{addEventListener(){}},state:{},Date,URLSearchParams,
  tag:x=>x,label:x=>x,esc:x=>String(x??''),n:x=>x,dt:x=>x,ms:x=>x,short:x=>x,icon:()=>'',
  head:(...x)=>x.join(''),table:(headers,rows)=>rows.join(''),empty:(...x)=>x.join('')};
 vm.createContext(context);
 vm.runInContext(fs.readFileSync(path.join(__dirname,'../error-center.js'),'utf8')+'\nglobalThis.errors=ErrorCenter;',context);
 assert.equal(context.errors.cause({reason:'cybersecurity_risk'}),'网络安全风险拦截');
 const row={code:'upstream_content_policy_violation',cause:'cybersecurity_risk',stage:'upstream_response',upstream_status:200,count:1,accounts:1,sessions:1};
 const html=await context.errors.page(async()=>({from:'2026-09-23T01:00:00Z',to:'2026-09-23T02:00:00Z',bucket_seconds:3600,trend:[],options:{accounts:[],keys:[]},summary:{total:1,groups:1,accounts:1,sessions:1},groups:[row],items:[{...row,id:'test',created_at:'2026-09-23T01:30:00Z',model:'gpt-6-astra',upstream_attempts:1}]}));
 assert.match(html,/上游内容策略拒绝/);
 assert.match(html,/网络安全风险拦截/);
 assert.match(html,/200 · 请求失败/);
});
