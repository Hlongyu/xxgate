'use strict';
const CacheMetrics = (() => {
 const dropThreshold = 20;
 function measure(r) {
  if(r.kind==='search')return {rate:null,reason:'Search 不适用'};
  const u=r.usage||{},input=u.input_tokens,cached=u.cached_input_tokens;
  if(!u.complete)return {rate:null,reason:['queued','inflight'].includes(r.state)?'等待用量':'用量不完整'};
  if(input==null||cached==null)return {rate:null,reason:'缓存用量未知'};
  if(!Number.isSafeInteger(input)||!Number.isSafeInteger(cached)||input<0||cached<0||cached>input)return {rate:null,reason:'用量异常'};
  if(input===0)return {rate:null,reason:'无输入 Token'};
  return {rate:cached/input*100,input,cached,uncached:input-cached};
 }
 function contextChanges(r,p) {
  const changes=[];
  if(r.account_id!==p.account_id)changes.push('账户变更');
  if(r.binding_id!==p.binding_id||r.binding_generation!==p.binding_generation)changes.push('绑定变更');
  if(r.provider!==p.provider||(r.upstream_model||r.model)!==(p.upstream_model||p.model))changes.push('模型变更');
  return changes;
 }
 function compare(r) {
  const current=measure(r),p=r.cache_previous;
  if(current.rate==null)return {...current,delta:null,drop:false};
  if(r.stateless)return {...current,delta:null,drop:false,reason:'无会话标识 · 独立请求'};
  if(!p)return {...current,delta:null,drop:false,reason:'无历史基准'};
  const changes=contextChanges(r,p),previous=measure(p);
  if(changes.length)return {...current,delta:null,drop:false,reason:changes.join(' · '),changes,previous};
  // Concurrent historical requests cannot establish a warm-cache ordering.
  if(!p.finished_at||!r.created_at||Date.parse(p.finished_at)>Date.parse(r.created_at))return {...current,delta:null,drop:false,reason:'前次请求尚未结束'};
  if(previous.rate==null)return {...current,delta:null,drop:false,reason:'前次用量不可比'};
  const delta=current.rate-previous.rate;
  return {...current,previous,delta,drop:delta<=-dropThreshold+1e-9};
 }
 function summarize(items) {
  const valid=items.map(measure).filter(m=>m.rate!=null);
  const input=valid.reduce((s,m)=>s+m.input,0),cached=valid.reduce((s,m)=>s+m.cached,0);
  return {rate:input?cached/input*100:null,minimum:valid.length?Math.min(...valid.map(m=>m.rate)):null,
   measured:valid.length,unknown:items.filter(r=>r.kind!=='search').length-valid.length,
   drops:items.filter(r=>compare(r).drop).length,input,cached};
 }
 return {dropThreshold,measure,compare,summarize,contextChanges};
})();
if(typeof module!=='undefined')module.exports=CacheMetrics;
