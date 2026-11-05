(function(root,factory){const api=factory();if(typeof module==='object'&&module.exports)module.exports=api;else root.RuntimeTelemetry=api;})(globalThis,function(){
 'use strict';
 const finite=v=>typeof v==='number'&&Number.isFinite(v);
 const number=(v,d=1)=>finite(v)?v.toFixed(d):'—';
 const mib=v=>finite(v)?(v/1048576).toFixed(1):'—';
 const clock=t=>new Date(t).toLocaleTimeString('zh-CN',{hour12:false});
 function merge(previous,next){const map=new Map(previous.map(s=>[s.at_ms,s]));for(const s of next)if(finite(s.at_ms))map.set(s.at_ms,s);const ordered=[...map.values()].sort((a,b)=>a.at_ms-b.at_ms);const end=ordered.at(-1)?.at_ms||0;return ordered.filter(s=>s.at_ms>end-300000).slice(-300);}
 function path(samples,key,max){
  const end=samples.at(-1)?.at_ms||0,start=end-300000;let open=false,result='';
  for(let i=0;i<samples.length;i++){const s=samples[i],v=s[key];if(!finite(v)){open=false;continue;}if(i&&s.at_ms-samples[i-1].at_ms>3500)open=false;
   const x=Math.max(0,Math.min(480,(s.at_ms-start)/300000*480)),y=112-Math.min(1,Math.max(0,v/max))*104;
   result+=(open?'L':'M')+x.toFixed(2)+' '+y.toFixed(2)+' ';open=true;
  }return result.trim();
 }
 function graph(samples,series,max,label){return `<svg viewBox="0 0 480 120" preserveAspectRatio="none" role="img" aria-label="${label}"><path class="telemetry-grid" d="M0 8H480M0 60H480M0 112H480"/>${series.map(([key,color])=>`<path d="${path(samples,key,max)}" fill="none" stroke="${color}" stroke-width="2" vector-effect="non-scaling-stroke"/>`).join('')}</svg>`;}
 function panel(){return '<section class="panel telemetry-panel" aria-label="实时资源监测"><div class="panel-head"><div><h2>实时资源</h2><p>当前监测节点 · 最近 5 分钟 · 每秒采样，每 2 秒更新 · 不受账户筛选影响</p></div><span id="telemetry-status" role="status">正在连接</span></div><div id="telemetry-content" class="telemetry-content"><p class="muted">等待资源采样…</p></div></section>';}
 function content(samples){const s=samples.at(-1);if(!s)return '<p class="muted">等待首个资源采样；重启后历史重新开始。</p>';
  const mem=samples.map(p=>({...p,memory_mib:finite(p.memory_bytes)?p.memory_bytes/1048576:null}));
  const memMax=Math.max(1,...mem.map(p=>p.memory_mib||0),finite(s.memory_limit_bytes)?s.memory_limit_bytes/1048576:0);
  const cpuMax=Math.max(100,...samples.map(p=>p.cpu_capacity_percent||0));
  const queueMax=Math.max(1,...samples.flatMap(p=>[p.inflight||0,p.queued||0]));
  const cpuHint=s.probe_error?'CPU / 容器内存采样不可用；未知值不会显示为零。':`单核口径 ${number(s.cpu_single_core_percent)}% · 可用 ${number(s.cpu_limit_cores,2)} 核`;
  return `<div class="telemetry-charts"><article><div class="telemetry-title"><h3>CPU 配额占用</h3><strong>${number(s.cpu_capacity_percent)}<small>%</small></strong></div><p>${cpuHint}</p><div class="telemetry-chart">${graph(samples,[['cpu_capacity_percent','#278052']],cpuMax,'CPU 配额占用趋势')}</div><div class="telemetry-axis"><span>5 分钟前</span><span>纵轴 0–${number(cpuMax,0)}%</span><span>现在</span></div></article><article><div class="telemetry-title"><h3>容器内存</h3><strong>${mib(s.memory_bytes)}<small>MiB</small></strong></div><p>配额 ${mib(s.memory_limit_bytes)} MiB · 含页缓存和网络缓冲</p><div class="telemetry-chart">${graph(mem,[['memory_mib','#5179a0']],memMax,'容器内存趋势')}</div><div class="telemetry-axis"><span>5 分钟前</span><span>纵轴 0–${number(memMax,0)} MiB</span><span>现在</span></div></article><article><div class="telemetry-title"><h3>执行中 / 排队</h3><strong>${number(s.inflight,0)}<small>/ ${number(s.queued,0)}</small></strong></div><p><i class="telemetry-legend inflight"></i>执行中 <i class="telemetry-legend queued"></i>排队</p><div class="telemetry-chart">${graph(samples,[['inflight','#278052'],['queued','#b78535']],queueMax,'执行中与排队请求趋势')}</div><div class="telemetry-axis"><span>5 分钟前</span><span>纵轴 0–${queueMax} 请求</span><span>现在</span></div></article></div><div class="telemetry-detail"><span>接收 <b>${number(s.accepted_per_second)}</b> /s</span><span>完成 <b>${number(s.completed_per_second)}</b> /s</span><span>失败 <b>${number(s.failed_per_second)}</b> /s</span><span>CPU 限流周期 <b>${number(s.cpu_throttled_periods_percent)}%</b></span><span>采样延迟 <b>${number(s.sampler_delay_ms)} ms</b></span><span>宿主机 CPU <b>${number(s.host_cpu_percent)}%</b></span></div><p class="telemetry-note">CPU 配额占用达到 100% 表示当前实例已用满分配的 CPU；限流周期仅统计当前 cgroup 的配额限流。内存包含缓存，口径可能高于 docker stats。</p>`;
 }
 function create({read,eligible,node,now=()=>Date.now(),schedule=setTimeout,cancel=clearTimeout}){
  let samples=[],timer=null,busy=false,controller=null,generation=0,running=false;
  function paint(error){const status=node('telemetry-status'),target=node('telemetry-content');if(!target)return;target.innerHTML=content(samples);if(status){const last=samples.at(-1);const stale=!last||now()-last.at_ms>5000;status.textContent=error?'更新失败 · 保留上次数据':!last?'等待采样':stale?'采样已延迟 · '+clock(last.at_ms):'更新于 '+clock(last.at_ms);status.className=error||stale?'telemetry-stale':'telemetry-live';}}
  async function refresh(){if(!running||busy||!eligible())return;busy=true;const token=generation;controller=new AbortController();try{const since=samples.at(-1)?.at_ms;const data=await read('/telemetry'+(since?'?since='+since:''),controller.signal);if(token!==generation||!eligible())return;samples=merge(samples,data.samples||[]);paint(false);}catch(e){if(e.name!=='AbortError'&&token===generation&&eligible())paint(true);}finally{busy=false;controller=null;if(running&&eligible()){if(timer)cancel(timer);timer=schedule(refresh,2000);}}}
  return {mount(){running=true;if(timer)cancel(timer);timer=null;paint(false);void refresh();},stop(){running=false;generation++;if(timer)cancel(timer);timer=null;controller?.abort();},reset(){this.stop();samples=[];paint(false);},refresh};
 }
 return {panel,content,merge,path,create};
});
