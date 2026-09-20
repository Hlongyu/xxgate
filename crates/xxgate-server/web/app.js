'use strict';
const $ = s => document.querySelector(s);
const app = $('#app'), dialog = $('#dialog');
const state = {page:'dashboard', filter:{}, offset:0, auditOffset:0, models:[], prices:[], accounts:[], groups:[], keys:[], config:null};
const icons = {
 errors:'<path d="m12 3 10 18H2L12 3Z"/><path d="M12 9v5m0 3h.01"/>',
 cache:'<ellipse cx="12" cy="5" rx="8" ry="3"/><path d="M4 5v7c0 4 16 4 16 0V5M4 12v7c0 4 16 4 16 0v-7"/>',
 drop:'<path d="m3 6 6 6 4-4 8 10m-6 0h6v-6"/>',
 groups:'<rect x="3" y="3" width="7" height="7" rx="2"/><rect x="14" y="3" width="7" height="7" rx="2"/><rect x="3" y="14" width="7" height="7" rx="2"/><rect x="14" y="14" width="7" height="7" rx="2"/>',
 trash:'<path d="M3 6h18M9 6V3h6v3M5 6l1 15h12l1-15M10 10v7m4-7v7"/>',

 dashboard:'<rect x="3" y="3" width="7" height="7" rx="1"/><rect x="14" y="3" width="7" height="7" rx="1"/><rect x="3" y="14" width="7" height="7" rx="1"/><rect x="14" y="14" width="7" height="7" rx="1"/>',
 accounts:'<circle cx="9" cy="8" r="3"/><path d="M3 21v-3a6 6 0 0 1 12 0v3m1-17a3 3 0 0 1 0 6m2 5a6 6 0 0 1 3 5"/>',
 usage:'<path d="M4 20h17M7 15v-5m5 5V5m5 10V8"/>',
 requests:'<path d="M8 5h13M8 12h13M8 19h13M3 5h.01M3 12h.01M3 19h.01"/>',
 models:'<path d="m12 3 9 5-9 5-9-5 9-5Zm-9 9 9 5 9-5M3 16l9 5 9-5"/>',
 keys:'<circle cx="8" cy="8" r="5"/><path d="m12 12 9 9m-6-6 3-3m0 6 3-3"/>',
 settings:'<path d="M4 7h16M4 17h16"/><circle cx="9" cy="7" r="3" fill="currentColor"/><circle cx="15" cy="17" r="3" fill="currentColor"/>',
 audit:'<path d="M5 3h10l4 4v14H5V3Zm10 0v5h4M8 12h8m-8 4h8"/>',
 refresh:'<path d="M20 8a9 9 0 1 0 1 8M20 3v6h-6"/>',
 close:'<path d="m6 6 12 12M6 18 18 6"/>',
 logout:'<path d="M10 4H4v16h6m6-13 5 5-5 5m-7-5h12"/>',
 arrow:'<path d="M4 12h16m-6-6 6 6-6 6"/>',
 plus:'<path d="M12 4v16M4 12h16"/>',
 clock:'<circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/>',
 calendar:'<rect x="3" y="5" width="18" height="16" rx="4"/><path d="M7 3v4m10-4v4M3 11h18m-13 5h3"/>',
 wallet:'<path d="M20 8V6a2 2 0 0 0-2-2H6a3 3 0 0 0 0 6h15v9a2 2 0 0 1-2 2H6a3 3 0 0 1-3-3V7"/><path d="M21 13h-5v5h5m-3-2.5h.01"/>',
 trend:'<path d="m3 17 6-6 4 4 8-10m-6 0h6v6"/>',
 activity:'<path d="M3 12h4l3-8 4 16 3-8h4"/>',
 queue:'<path d="M8 6h13M8 12h10M8 18h7"/><circle cx="3" cy="6" r=".5"/><circle cx="3" cy="12" r=".5"/><circle cx="3" cy="18" r=".5"/>',
 shield:'<path d="m12 3 8 3v6c0 5-8 9-8 9s-8-4-8-9V6l8-3Z"/><path d="m8 12 3 3 5-6"/>',
 check:'<path d="m5 12 4 4L19 6"/>',
 edit:'<path d="m15 5 4 4M4 20l5-1L21 7a2.8 2.8 0 0 0-4-4L5 15l-1 5Z"/>',
 download:'<path d="M12 3v12m-4-4 4 4 4-4M4 16v3a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2v-3"/>',
};
function icon(n){return `<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">${icons[n]||icons.requests}</svg>`;}
function logo(){return '<svg class="mark" viewBox="0 0 30 30" fill="none" aria-hidden="true"><path d="m3 5 10 10L3 25M17 5l10 10-10 10" stroke="currentColor" stroke-width="4" stroke-linejoin="round"/></svg>'}
function esc(x){return String(x??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));}
function n(x){return Number(x||0).toLocaleString('zh-CN');}
function money(x){return x==null?'—':Number(x).toLocaleString('zh-CN',{minimumFractionDigits:2,maximumFractionDigits:8});}
function dt(x){return x?new Date(x).toLocaleString('zh-CN',{hour12:false}):'—';}
function ms(x){return x==null?'—':x<1000?`${x} ms`:`${(x/1000).toFixed(2)} s`;}
function short(x){return x?String(x).slice(0,8):'—';}
const labels={account_deleted:'删除账户',key_deleted:'删除 Key',encrypted_reasoning_recovery:'清理加密推理并准备恢复',upstream_attempt_headers:'收到上游尝试响应头',client_source_not_allowed:'同组账户不允许此客户端来源',search_price_updated:'更新 Search 全局单价',unpriced_search:'未设置 Search 单价',not_charged:'不计费',quota_reset_unconfirmed:'重置结果待核实',quota_reset_started:'开始额度重置',quota_reset_finished:'额度重置结果',group_created:'创建分组',group_updated:'修改分组',group_deleted:'删除分组',no_available_account:'同组无可用账户',account_busy:'账户并发已满',gateway_busy:'网关并发已满',session_busy:'会话正在处理请求',key_group_changed:'Key 分组已变更',request_rewritten:'记录请求改写',models_synchronized:'已同步模型',models_sync_failed:'模型同步失败',oauth_request_failed:'授权请求失败',rejected:'已拒绝',request_rejected:'拒绝请求',completed:'已完成',failed:'失败',queued:'排队中',inflight:'处理中',cancelled:'已取消',interrupted:'进程中断',admin_disabled:'管理员停用',quota5h_exhausted:'5h 额度耗尽',quota7d_exhausted:'7days 额度耗尽',quota_exhausted:'额度耗尽',oauth_invalid:'授权失效',priced:'已计价',partial:'部分用量',not_executed:'未调用上游',cache_usage_unknown:'缓存用量未知',usage_unknown:'用量未知',unpriced:'未配置价格',tier_unknown:'实际档位未知',unpriced_fast:'未配置 Fast 倍率',image_unpriced:'图片未完整计价',invalid_usage:'用量异常',request_accepted:'接收请求',dispatched:'派发上游',request_finished:'请求结束',delivery_interrupted:'下游连接中断',session_bound:'会话绑定',session_migrated:'会话迁移',binding_created:'建立绑定',settings_changed:'更新配置',account_created:'创建账户',account_updated:'更新账户',account_enabled:'启用账户',account_disabled:'停用账户',credentials_updated:'更新授权',key_created:'创建 Key',key_updated:'更新 Key',model_updated:'更新模型',price_updated:'更新价格',admin_login:'管理员登录'};
function label(x){return labels[x]||x||'—';}
function tag(x){return `<span class="tag ${x==='completed'||x==='enabled'?'good':x==='queued'||x==='inflight'?'wait':['failed','interrupted','rejected'].includes(x)?'bad':''}">${esc(x==='enabled'?'已启用':x==='disabled'?'已停用':label(x))}</span>`;}
async function api(path,method='GET',body,signal){
 const r=await fetch('/api/admin'+path,{method,signal,credentials:'same-origin',headers:{'content-type':'application/json','x-xxgate-csrf':'1'},body:body===undefined?undefined:JSON.stringify(body)});
 const data=r.status===204?{}:await r.json();signal?.throwIfAborted();
 if(!r.ok){if(r.status===401&&path!='/login')loginPage();throw new Error(data.error?.message||`请求失败 (${r.status})`);}
 return data;
}
let toastTimer,oauthTimer,pendingBrowserFlow,pageLoad,detailLoad,dialogVersion=0,lastPageContent;
function toast(text){$('#toast').textContent=text;$('#toast').classList.add('visible');clearTimeout(toastTimer);toastTimer=setTimeout(()=>$('#toast').classList.remove('visible'),4000);}
function modal(title,body,{subtitle='',drawer=false}={}){dialogVersion++;detailLoad?.abort();hideBillingTooltip();clearInterval(oauthTimer);dialog.className=drawer?'drawer':'';$('#dialog-content').innerHTML=`<header class="dialog-head"><div><h2>${esc(title)}</h2>${subtitle?`<small>${esc(subtitle)}</small>`:''}</div><button class="icon ghost" data-action="close" aria-label="关闭">${icon('close')}</button></header><div class="dialog-body">${body}</div>`;if(!dialog.open)dialog.showModal();}
function close(){dialogVersion++;detailLoad?.abort();clearInterval(oauthTimer);if(pendingBrowserFlow){api('/oauth/browser/'+pendingBrowserFlow,'DELETE').catch(()=>{});pendingBrowserFlow=null;}dialog.close();$('#dialog-content').replaceChildren();}
dialog.addEventListener('close',()=>{dialogVersion++;detailLoad?.abort();clearInterval(oauthTimer);if(pendingBrowserFlow){api('/oauth/browser/'+pendingBrowserFlow,'DELETE').catch(()=>{});pendingBrowserFlow=null;}});
function field(title,name,value='',type='text',hint='',attrs=''){return `<label class="field">${title}<input name="${name}" type="${type}" value="${esc(value)}" ${attrs}>${hint?`<small>${hint}</small>`:''}</label>`;}
function formActions(text='保存'){return `<div class="form-actions"><span class="text-error" role="alert"></span><button type="button" data-action="close">取消</button><button class="primary" type="submit">${text}</button></div>`;}
function empty(title,description,action=''){return `<div class="empty">${icon('requests')}<strong>${title}</strong><p>${description}</p>${action}</div>`;}
function head(title,desc,actions=''){return `<div class="page-head"><div><div class="eyebrow">WORKSPACE / ${esc(state.page.toUpperCase())}</div><h1>${title}</h1>${desc?`<p>${desc}</p>`:''}</div><div class="actions">${actions}<button class="icon" data-action="refresh" aria-label="刷新">${icon('refresh')}</button></div></div>`;}
function table(headers,rows){return `<div class="table-wrap"><table><thead><tr>${headers.map(h=>`<th>${h}</th>`).join('')}</tr></thead><tbody>${rows.join('')}</tbody></table></div>`;}
function fact(rows){return `<dl class="facts">${rows.map(([a,b])=>`<dt>${a}</dt><dd>${b}</dd>`).join('')}</dl>`;}
function footer(){return '<div class="footer-note"><span>XXGate · Codex 0.153.4 · HTTP / SSE</span><span>费用单位 CNY · 未知用量不按零用量计算</span></div>';}
function loginPage(){
 state.auth=false;pageLoad?.abort();lastPageContent=null;close();app.innerHTML=`<div class="login"><aside class="login-side"><div class="brand">${logo()}XXGate</div><div class="login-visual"><svg viewBox="0 0 450 190" fill="none" aria-hidden="true"><path d="M20 35h100l70 60h90l70-60h80M20 95h410M20 155h100l70-60h90l70 60h80" stroke="currentColor" stroke-width="1"/><circle cx="230" cy="95" r="29" stroke="#8cc8a2"/><circle cx="230" cy="95" r="7" fill="#8cc8a2"/><circle cx="20" cy="35" r="4" fill="currentColor"/><circle cx="20" cy="95" r="4" fill="currentColor"/><circle cx="20" cy="155" r="4" fill="currentColor"/></svg><h1>每个账户，<br>每一次请求。</h1><p>统一管理账户与授权，<br>追踪请求、额度和用量费用。</p></div><small>XXGATE / ADMINISTRATOR CONSOLE</small></aside><section class="login-form"><div class="login-inner"><h2>管理员登录</h2><p>登录以管理你的账户网关。</p><form data-form="login"><label class="field">管理员密码<input name="password" type="password" autocomplete="current-password" required placeholder="输入管理员密码"></label><button class="primary" type="submit">登录工作台</button><p class="text-error" role="alert"></p></form><footer>使用首次启动时设置的管理员密码。<br>对外 API 调用使用独立的网关 Key。</footer></div></section></div>`;
}
const pages=[['dashboard','总览'],['accounts','账户'],['groups','分组'],['requests','请求追踪'],['errors','错误调查'],['usage','用量费用'],['models','模型与价格'],['keys','API Keys'],['settings','运行配置'],['audit','操作审计']];
function shell(){state.auth=true;app.innerHTML=`<aside class="sidebar"><div class="brand">${logo()}XXGate</div><div class="nav-label">网关管理</div><nav class="nav">${pages.map(([id,text],i)=>`${id==='keys'?'<div class="nav-label">系统</div>':''}<button aria-label="${text}" title="${text}" data-page="${id}" class="${id===state.page?'active':''}">${icon(id)}<span>${text}</span></button>`).join('')}</nav><div class="sidebar-foot"><span class="avatar">AD</span><div>Administrator<small>管理员</small></div><button class="icon ghost" data-action="logout" aria-label="退出登录">${icon('logout')}</button></div></aside><div class="shell"><header class="topbar"><div>工作空间<span class="divider">/</span><strong id="breadcrumb">总览</strong></div><div class="service"><i class="dot"></i><span>管理控制台</span><span class="divider">·</span><span id="updated">正在同步</span></div></header><main id="main"></main></div>`;render();}
async function render(quiet=false){
 if(!state.auth||(quiet&&(pageLoad||billingTip||document.hidden)))return;
 pageLoad?.abort();const controller=new AbortController();pageLoad=controller;
 const page=state.page,title=pages.find(p=>p[0]===page)[1],read=(path,method,body)=>api(path,method,body,controller.signal);
 hideBillingTooltip();
 if(!quiet){lastPageContent=null;$('#breadcrumb').textContent=title;$('#main').innerHTML=head(title,'')+'<div class="notice" role="status">正在加载…</div>';}
 try{
  const loaders={dashboard:dashboardPage,accounts:accountsPage,groups:groupsPage,requests:requestsPage,errors:ErrorCenter.page,usage:usagePage,models:modelsPage,keys:keysPage,settings:settingsPage,audit:auditPage};
  const content=await loaders[page](read);
  if(state.auth&&pageLoad===controller&&state.page===page){
   if(quiet&&(dialog.open||billingTip||document.querySelector('main input:focus,main select:focus')))return;
   const html=content+(['accounts','groups'].includes(page)?'':footer());
   if(html!==lastPageContent){$('#main').innerHTML=html;lastPageContent=html;}
   $('#updated').textContent=new Date().toLocaleTimeString('zh-CN',{hour12:false});
  }
 }catch(e){if(e.name!=='AbortError'&&!quiet&&state.auth&&pageLoad===controller){$('#main').innerHTML=head(title,'')+`<div class="notice error" role="alert">${esc(e.message)} · 可点击刷新重试。</div>`;}}
 finally{if(pageLoad===controller)pageLoad=null;}
}
function navigate(page){state.page=page;document.querySelectorAll('[data-page]').forEach(x=>x.classList.toggle('active',x.dataset.page===page));location.hash=page;render();}
async function dashboardPage(api){
 const q=state.accountFilter?`?account_id=${encodeURIComponent(state.accountFilter)}`:'';
 const [d,a]=await Promise.all([api('/dashboard'+q),api('/accounts')]);state.accounts=a.items;
 const s=d.summary,r=d.runtime.queue;
 const select=`<select class="inline-select" aria-label="统计账户" id="dashboard-account"><option value="">全部账户</option>${a.items.map(({account:x})=>`<option value="${x.id}" ${state.accountFilter===x.id?'selected':''}>${esc(x.name)}</option>`).join('')}</select>`;
 const hourly=d.hourly||[];const max=Math.max(1,...hourly.map(x=>x.requests));const hours=Array.from({length:24},(_,i)=>{let t=new Date();t.setMinutes(0,0,0);t.setHours(t.getHours()-23+i);return hourly.find(x=>new Date(x.hour).getTime()===t.getTime());});
 const bars=hours.map((x,i)=>`<rect class="chart-bar" x="${i*20+2}" y="${145-(x?x.requests/max*130:2)}" width="12" height="${x?Math.max(2,x.requests/max*130):2}" rx="2"><title>${x?dt(x.hour)+' · '+x.requests+' 次请求':'无请求'}</title></rect>`).join('');
 return head('网关总览','查看账户运行状态、请求用量与人民币费用。',select)+`<section class="metrics"><div class="metric"><label>累计请求</label><strong>${n(s.requests)}</strong><small>${n(s.completed)} 已完成 · ${n(s.failed)} 失败</small></div><div class="metric"><label>输入 / 输出 Token</label><strong>${n(Number(s.input_tokens)+Number(s.output_tokens))}</strong><small>输入 ${n(s.input_tokens)} / 输出 ${n(s.output_tokens)}</small></div><div class="metric"><label>累计费用</label><strong><em>¥</em>${money(s.cny)}</strong><small>${n(s.unpriced)} 次未完整计价</small></div><div class="metric"><label>当前并发 / 排队</label><strong>${n(r.inflight)} <span class="muted">/ ${n(r.queued)}</span></strong><small>全部账户 · ${r.paused?'派发已暂停':'调度正常'}</small></div></section><div class="two-col"><section class="panel"><div class="panel-head"><div><h2>请求活动</h2><p>过去 24 小时 · 每小时已结束请求</p></div><span class="tag">24 HOURS</span></div><div class="panel-body"><div class="chart"><svg viewBox="0 0 480 150" preserveAspectRatio="none" role="img" aria-label="过去24小时请求柱状图">${bars}</svg></div><div class="chart-labels"><span>24 小时前</span><span>12 小时前</span><span>现在</span></div></div></section><section class="panel"><div class="panel-head"><h2>运行状态</h2>${tag(r.paused?'failed':'enabled')}</div><div class="panel-body runtime-list">${[['可用账户',`${a.items.filter(x=>x.account.enabled).length} / ${a.items.length}`],['内存预算占用',`${(d.runtime.memory_bytes/1048576).toFixed(1)} MB`],['当前配置',`v${d.runtime.config_version}`],['用量不完整',`${n(s.incomplete_usage)} 次`],['已生成图片',n(s.images)],['平均总耗时',ms(Number(s.average_ms).toFixed(0))]].map(([k,v])=>`<div class="runtime-line"><span>${k}</span><strong>${v}</strong></div>`).join('')}</div></section></div><section class="panel"><div class="panel-head"><h2>模型用量</h2><button class="small ghost" data-page="models">管理价格 →</button></div>${d.models.length?table(['模型','请求','已计价费用'],d.models.map(m=>`<tr><td class="mono">${esc(m.model)}</td><td>${n(m.requests)}</td><td>¥ ${money(m.cny)}</td></tr>`)):empty('还没有请求记录','添加账户、模型与 API Key 后即可开始接入。','<button data-page="accounts">管理账户</button>')}</section>`;
}
function quota(windows,minutes){
 const entry=windows.find(x=>(x.window||x).window_minutes===minutes&&(x.window||x).pool==='codex');
 if(!entry)return '<span class="muted">额度未知 · 到期时间未知</span>';
 const w=entry.window||entry;
 return `<div class="quota" title="采集：${esc(dt(w.observed_at))}"><div class="quota-label"><span>${Number(w.used_percent).toFixed(1)}% 已用${entry.stale?' · 待更新':''}</span></div><meter min="0" max="100" low="80" high="95" optimum="0" value="${Math.min(100,w.used_percent)}" aria-label="${minutes===300?'5h':'7days'}额度已使用比例"></meter><span class="quota-expiry">到期 ${w.resets_at?esc(dt(w.resets_at)):'未知'}</span></div>`;
}
function accountModels(a){
 const catalog=a.model_catalog?.synced_at?a.model_catalog.models.map(m=>m.id):a.models;
 return catalog.filter(id=>(!a.models_restricted&&!a.models.length)||a.models.includes(id));
}
function modelChips(models){return models.map(m=>`<span class="model-chip">${icon('models')}<span>${esc(m)}</span></span>`).join('');}
function accountSwitch(a){return `<div class="account-toggle"><button class="toggle-switch" role="switch" aria-checked="${a.enabled}" aria-label="启用账户 ${esc(a.name)}" data-action="account-enabled" data-id="${a.id}" data-enabled="${!a.enabled}"><span></span></button><span>${a.enabled?'已启用':'已停用'}</span></div>${!a.enabled&&a.disable_reason?`<div class="sub account-reason">${esc(label(a.disable_reason))}</div>`:''}`;}
function accountAmount(value){return value==null?'—':Number(value).toLocaleString('zh-CN',{minimumFractionDigits:2,maximumFractionDigits:2});}
function spendingPeriod(period,compact=false){if(!period||period.cny==null)return '<span class="muted" title="尚未确认当前额度周期，请采集额度">周期未知</span>';const hint=`本周期 ${dt(period?.starts_at)} 至 ${dt(period?.resets_at)} · 精确金额 ¥ ${money(period?.cny??'0')} · ${period?.requests??0} 次请求${period?.unpriced?'；其中 '+period.unpriced+' 次未完整计价，金额未计入':''}`;return `<span class="amount-chip" tabindex="0" title="${esc(hint)}" aria-label="${esc(hint)}">${icon('wallet')}<span>¥ ${compact?accountAmount(period?.cny??'0'):money(period?.cny??'0')}</span>${period?.unpriced?'<sup class="partial-mark">*</sup>':''}</span>`;}
function cycleTokenCount(value){
 if(value==null)return '—';
 const amount=Number(value),unit=amount>=1e9?'B':amount>=1e6?'M':'';
 return (unit?(amount/(unit==='B'?1e9:1e6)).toLocaleString('en-US',{maximumFractionDigits:2}):n(value))+unit;
}
function cycleUsage(period){
 const known=period?.status==='current_cycle',value=x=>known&&x!=null?n(x):'—';
 const tokens=(label,x)=>`<span title="${label} ${value(x)} Token">${label} <b>${known?cycleTokenCount(x):'—'}</b> <small>Token</small></span>`;
 return `<div class="cycle-usage" title="${period?.missing_tokens?'部分请求缺少 Token 记录，仅汇总已知用量':'当前额度周期内已结束请求的用量'}"><span>请求 <b>${value(period?.requests)}</b> 次</span>${tokens('输入',period?.input_tokens)}${tokens('输出',period?.output_tokens)}${period?.missing_tokens?'<span class="partial-mark">*</span>':''}</div>`;
}
function weeklyEstimate(e,compact=false){
 if(e?.total_cny==null)return `<span class="estimate-value estimate-empty" title="需要同一有效周额度周期内至少 1% 的消耗变化和网关计价记录">${icon('trend')}<span>样本不足</span></span>`;
 const hint=`样本 ${dt(e.sample_from)} 至 ${dt(e.sample_to)}；已计价 ¥${money(e.sample_cny)} ÷ 额度消耗 ${Number(e.used_percent_delta).toFixed(1)}% × 100。按样本价格与模型组合估算，账户在网关外使用或未计价请求会使结果偏低${e.sample_stale?'；使用最近历史采集样本':''}${e.unpriced?'；样本中有 '+e.unpriced+' 次未完整计价':''}。`;
 return `<span class="weekly-estimate estimate-value" tabindex="0" title="${esc(hint)}" aria-label="预计周额度金额 ¥ ${money(e.total_cny)}；${esc(hint)}">${icon('trend')}<span><small>≈ ¥</small> ${compact?accountAmount(e.total_cny):money(e.total_cny)}</span></span><span class="sample-chip">${icon('usage')}<span>${Number(e.used_percent_delta).toFixed(1)}% 本周期 · ${n(e.requests)} 次${e.sample_stale?' · 待采集':''}</span></span>`;
}
async function accountsPage(api){
 const [d]=await Promise.all([api('/accounts'),loadGroups(api)]);state.accounts=d.items;
 const visible=d.items.filter(({account:a})=>!state.accountGroupFilter||a.group_ids.includes(state.accountGroupFilter));
 const cards=visible.map(({account:a,email,quotas:q,spending:s,resets:z})=>{const r=d.runtime.accounts.find(x=>x.id===a.id)||{},models=accountModels(a);return `<article class="account-row" aria-label="账户 ${esc(a.name)}">
  <div class="account-row-identity"><button class="ghost account-title" data-action="account-detail" data-id="${a.id}">${esc(a.name)}</button><span class="account-email" title="${esc(email||'邮箱未知')}">${esc(email||'邮箱未知')}</span><div class="account-row-meta">${groupChips(a.group_ids)}${a.codex_only?'<span class="codex-only-label">仅 Codex</span>':''}<span title="处理中 / 并发上限">并发 ${n(r.inflight||0)}/${n(a.max_inflight)} · 排队 ${n(r.queued||0)}</span></div></div>
  <div class="account-row-periods">${[[300,'5h',s?.last_5h],[10080,'7days',s?.last_7d]].map(([minutes,label,period])=>`<section class="account-row-period"><div class="account-row-heading"><h3>${label}</h3>${spendingPeriod(period,true)}</div>${quota(q,minutes)}${cycleUsage(period)}</section>`).join('')}</div>
  <section class="account-row-estimate"><h3>本周期周额度估算</h3>${weeklyEstimate(s?.weekly_estimate,true)}</section>
  <div class="account-row-controls">${accountSwitch(a)}<div class="actions"><button class="small ghost" data-action="account-detail" data-id="${a.id}">详情</button><button class="icon" data-action="account-edit" data-id="${a.id}" aria-label="设置账户 ${esc(a.name)}" title="账户设置">${icon('settings')}</button><button class="icon ghost danger" data-action="account-delete" data-id="${a.id}" aria-label="删除账户 ${esc(a.name)}" title="删除账户">${icon('trash')}</button></div></div>
  <div class="account-row-footer"><button class="small ghost" data-action="account-models-open" data-id="${a.id}" title="${esc(models.join('、'))}">${n(models.length)} 个模型 · 编辑</button><button class="small ghost" data-action="reset-open" data-id="${a.id}">${z?.pending?'重置结果待核实':z?.snapshot?'可用重置 '+n(z.usable_count)+' 次':'查询重置次数'} ${icon('refresh')}</button></div>
 </article>`;});
 return `<div class="accounts-workspace">${head('账户','',`<button data-action="account-import">${icon('download')}导入授权</button><button class="primary" data-action="oauth-start">${icon('plus')}添加账户</button>`)}<div class="accounts-summary">${groupFilter('account-group-filter',state.accountGroupFilter)}<span class="account-count">${icon('accounts')}<span>${n(visible.length)} 个账户</span></span><span class="account-count enabled-count">${icon('check')}<span>${n(visible.filter(x=>x.account.enabled).length)} 已启用</span></span></div><div class="account-list">${cards.length?cards.join(''):empty('暂无账户','','')}</div></div>`;
}
async function accountModelEdit(id){
 const d=await api('/accounts/'+id),a=d.account;state.modelEditor=a;
 const candidates=[...new Set([...(a.model_catalog?.models||[]).map(m=>m.id),...a.models])].sort(),selected=accountModels(a);
 modal('模型范围',`<form data-form="account-models" data-id="${a.id}"><div class="model-selection-head"><span id="model-selection-count" class="account-count">${icon('models')}<span>${selected.length} / ${candidates.length} 已选</span></span><div class="actions"><button class="small ghost" type="button" data-action="account-models-all">${icon('check')}全选</button><button class="small ghost" type="button" data-action="account-models-none">${icon('close')}清空</button><button class="small" type="button" data-action="account-models-refresh" data-id="${a.id}">${icon('refresh')}同步上游</button></div></div><div class="model-selection">${candidates.map(m=>`<label class="model-option"><input type="checkbox" name="account_model" value="${esc(m)}" ${selected.includes(m)?'checked':''}><span class="mono">${esc(m)}</span></label>`).join('')||'<p class="muted">暂无模型</p>'}</div>${formActions('保存模型范围')}</form>`,{subtitle:a.name});
}
function updateModelSelection(){const boxes=dialog.querySelectorAll('input[name="account_model"]');if($('#model-selection-count'))$('#model-selection-count').innerHTML=icon('models')+'<span>'+[...boxes].filter(b=>b.checked).length+' / '+boxes.length+' 已选</span>';}
function uncachedTokens(usage){
 const input=usage?.input_tokens,cached=usage?.cached_input_tokens;
 return input===0?0:input==null||cached==null||cached>input?null:input-cached;
}
function firstTokenMs(r){return r.first_content_ms==null||r.queue_ms==null?null:r.queue_ms+r.first_content_ms;}
function tokensPerSecond(r){
 const output=r.usage?.output_tokens,duration=r.total_ms;
 return !r.usage?.complete||output==null||duration==null||duration<=0?null:output*1000/duration;
}
function reasoningLabel(effort){return {none:'无思考',minimal:'极低',low:'低',medium:'中',high:'高',xhigh:'超高',max:'最高',ultra:'极致'}[effort]||'—';}
function billingFactor(r){
 const actual=r.usage?.service_tier;
 if(['fast','priority'].includes(actual))return r.price?.fast_multiplier??null;
 if(!actual&&['fast','priority'].includes(r.requested_tier))return null;
 return r.valuation?.items?.length?1:null;
}
function billingTooltip(r){
 if(r.kind==='search'){const rate=r.search_price?.per_call,v=r.valuation;return `<div class="billing-head"><strong>Search 计费详情</strong></div><table><thead><tr><th>成功次数</th><th>单价（¥ / 次）</th><th>金额（¥）</th></tr></thead><tbody><tr><td>${n(r.usage?.search_calls)}</td><td>${money(rate)}</td><td>${money(v?.cny)}</td></tr></tbody></table><p>${esc(label(v?.status))} · 全局按次计费</p>`;}

 const v=r.valuation,lines=v?.items||[],standard=r.price?.standard,factor=billingFactor(r);
 const names={input:'非缓存输入',cached_input:'缓存输入',output:'输出',image:'图片',image_input:'图片输入',image_output:'图片输出'};
 const kinds=['input','output','cached_input',...lines.map(x=>x.kind).filter(x=>!['input','output','cached_input'].includes(x))];
 const rateKeys={input:'input_per_million',output:'output_per_million',cached_input:'cached_input_per_million',image:'per_image',image_input:'image_input_per_million',image_output:'image_output_per_million'};
 const rows=kinds.map(kind=>{const line=lines.find(x=>x.kind===kind),rate=standard?.[rateKeys[kind]];return `<tr><td>${esc(names[kind]||kind)}</td><td>${rate==null?'—':money(rate)}</td><td>${line?money(line.cny):'—'}</td></tr>`;}).join('');
 return `<div class="billing-head"><strong>计费详情</strong></div><table><thead><tr><th>项目</th><th>标准单价</th><th>金额（¥）</th></tr></thead><tbody>${rows}</tbody></table><div class="billing-unit">单价：¥ / 百万 Token；按张计费的图片为 ¥ / 张</div><div class="billing-total"><span>倍率 <b>${factor==null?'—':'×'+esc(Number(factor))}</b></span><strong>合计 ${v?.cny==null?'—':'¥ '+money(v.cny)}</strong></div><p>${esc(label(v?.status))} · 分项金额已包含倍率${factor==null?' · 倍率尚未确认':''}</p>`;
}
let billingTip,billingAnchor;
function hideBillingTooltip(){if(billingTip)billingTip.remove();billingTip=null;billingAnchor=null;}
function showBillingTooltip(anchor){
 if(billingAnchor===anchor)return;
 hideBillingTooltip();const template=anchor.querySelector('template');if(!template)return;
 billingTip=document.createElement('div');billingTip.id='billing-tooltip';billingTip.className='billing-tooltip';billingTip.setAttribute('role','tooltip');billingTip.innerHTML=template.innerHTML;document.body.append(billingTip);billingAnchor=anchor;
 const rect=anchor.getBoundingClientRect(),tip=billingTip.getBoundingClientRect();
 billingTip.style.left=Math.max(12,Math.min(rect.left+rect.width/2-tip.width/2,innerWidth-tip.width-12))+'px';
 billingTip.style.top=Math.max(12,rect.top>tip.height+20?rect.top-tip.height-10:Math.min(rect.bottom+10,innerHeight-tip.height-12))+'px';
}
app.addEventListener('pointerover',event=>{const anchor=event.target.closest('[data-billing]');if(anchor)showBillingTooltip(anchor);});
app.addEventListener('pointerout',event=>{const anchor=event.target.closest('[data-billing]');if(anchor&&!anchor.contains(event.relatedTarget))hideBillingTooltip();});
app.addEventListener('focusin',event=>{const anchor=event.target.closest('[data-billing]');if(anchor)showBillingTooltip(anchor);});
app.addEventListener('focusout',hideBillingTooltip);
window.addEventListener('scroll',hideBillingTooltip,true);window.addEventListener('resize',hideBillingTooltip);
document.addEventListener('keydown',event=>{if(event.key==='Escape')hideBillingTooltip();});
function clientSource(r){
 const origin=r.client_origin;
 if(!origin)return '<span class="client-source source-unrecorded">来源未记录</span>';
 const label=origin.source==='codex'?'Codex':'未知来源';
 const rules={codex_user_agent:'Codex User-Agent',codex_turn_metadata:'Codex turn 元数据',codex_client_metadata:'Codex 客户端元数据',unrecognized:'没有匹配的客户端特征'};
 return `<span class="client-source ${origin.source==='codex'?'source-codex':'source-unknown'}" title="${esc((rules[origin.rule]||origin.rule)+' · '+origin.evidence.join('、'))}">${icon(origin.source==='codex'?'shield':'requests')}${label}</span>`;
}
function requestType(r){
 if(r.kind==='compact')return '<span class="request-type request-type-responses">Compact · 非流式</span>';
 if(r.kind==='search')return '<span class="request-type request-type-search">Search 搜索</span>';
 const mode=r.stream===true?'流式':r.stream===false?'非流式':'';
 return `<span class="request-type request-type-responses">Responses${mode?' · '+mode:''}</span>`;
}
function cachePercent(value){return value==null?'—':value.toFixed(1)+'%';}
function traceChip(name,value,kind='usage'){return `<span class="trace-chip">${icon(kind)}<span>${esc(name)}</span><b>${esc(value)}</b></span>`;}
function cacheDelta(r,m=CacheMetrics.compare(r)){
 if(m.rate==null)return `<span class="cache-note">${esc(m.reason)}</span>`;
 if(m.delta==null)return `<span class="cache-note">${esc(m.reason)}</span>`;
 const delta=Math.abs(m.delta)<.05?0:m.delta;
 return `<span class="cache-delta ${m.drop?'is-drop':''}" title="与同 Key、会话、线程的前次已派发 Responses 请求比较；下降至少 ${CacheMetrics.dropThreshold} 个百分点时突出显示">${m.drop?icon('drop'):''}${delta===0?'持平':(delta>0?'↑ ':'↓ ')+Math.abs(delta).toFixed(1)+' 个百分点'}</span>`;
}
function cacheCell(r){
 const m=CacheMetrics.compare(r);
 return `<div class="cache-cell ${m.drop?'is-drop':''} ${m.rate==null?'is-unknown':''}"><div class="cache-cell-value">${icon('cache')}<strong>${cachePercent(m.rate)}</strong></div>${m.rate==null?'':`<div class="cache-track" role="meter" aria-label="缓存命中率" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${m.rate.toFixed(2)}"><svg viewBox="0 0 100 5" preserveAspectRatio="none" aria-hidden="true"><rect width="${m.rate}" height="5" rx="2.5"/></svg></div>`}${cacheDelta(r,m)}</div>`;
}
function cacheDetail(r){
 const m=CacheMetrics.compare(r),p=r.cache_previous;
 return `<section class="cache-detail ${m.drop?'is-drop':''}"><div class="cache-detail-main"><div><h3>${icon('cache')}缓存命中率</h3><strong class="cache-detail-rate">${cachePercent(m.rate)}</strong>${cacheDelta(r,m)}</div><div class="cache-detail-tokens">${traceChip('缓存输入',r.usage?.cached_input_tokens==null?'未知':n(r.usage.cached_input_tokens),'cache')}${traceChip('总输入',r.usage?.input_tokens==null?'未知':n(r.usage.input_tokens),'download')}${traceChip('非缓存输入',uncachedTokens(r.usage)==null?'未知':n(uncachedTokens(r.usage)),'usage')}</div></div>${m.rate==null?'':`<div class="cache-track"><svg viewBox="0 0 100 5" preserveAspectRatio="none" aria-hidden="true"><rect width="${m.rate}" height="5" rx="2.5"/></svg></div>`}<p>缓存输入 ÷ 总输入 Token${m.rate==null?' · '+esc(m.reason):' · '+n(m.cached)+' ÷ '+n(m.input)}</p>${p&&r.kind!=='search'?`<div class="cache-baseline"><span>前次请求 ${dt(p.created_at)} · ${cachePercent(CacheMetrics.measure(p).rate)}</span><button class="small ghost" data-action="request-detail" data-id="${esc(p.id)}">查看前次 ${icon('arrow')}</button></div>`:''}<p class="cache-help">${r.stateless?'本次未提供会话标识，按独立请求处理。缓存键独立于会话，命中率以实际用量为准，不与其他请求比较。':`同 Key、会话、线程比较；下降 ≥ ${CacheMetrics.dropThreshold} 个百分点时提示。账户、绑定或模型变化单独标记。上下文前缀变化、压缩或缓存过期也可能降低命中率。`}</p>${r.client_session_id?`<button class="small" data-action="request-session" data-session="${esc(r.client_session_id)}" data-key="${esc(r.key_id)}">${icon('activity')}查看此会话趋势</button>`:''}</section>`;
}
function cacheOverview(items){
 const s=CacheMetrics.summarize(items),ordered=[...items].reverse(),positions=new Map(),left=36,right=928,top=12,bottom=100;
 const points=ordered.map((r,i)=>{const m=CacheMetrics.compare(r),x=ordered.length===1?(left+right)/2:left+i/(ordered.length-1)*(right-left),y=m.rate==null?120:bottom-m.rate/100*(bottom-top);positions.set(r.id,{x,y,r,m});return {x,y,r,m};});
 const lines=points.map(({x,y,r,m})=>{const p=positions.get(r.cache_previous?.id);return p&&p.m.rate!=null&&m.delta!=null?`<line x1="${p.x}" y1="${p.y}" x2="${x}" y2="${y}" class="cache-line ${m.drop?'is-drop':''}"/>`:'';}).join('');
 const dots=points.map(({x,y,r,m})=>`<a href="#requests" data-action="request-detail" data-id="${esc(r.id)}" aria-label="${esc(dt(r.created_at)+' '+r.model+'，缓存命中率 '+cachePercent(m.rate)+(m.drop?'，明显下降':''))}"><title>${esc(dt(r.created_at)+' · '+r.model+' · '+cachePercent(m.rate)+' · '+(m.delta==null?m.reason:(m.delta>0?'+':'')+m.delta.toFixed(1)+' 个百分点')+' · 点击查看详情')}</title><circle cx="${x}" cy="${y}" r="12" class="cache-hit-target"/><circle cx="${x}" cy="${y}" r="${m.drop?5:3.8}" class="cache-point ${m.drop?'is-drop':''} ${m.rate==null?'is-unknown':''}"/></a>`).join('');
 return `<section class="cache-overview"><header><div><h2>${icon('activity')}${state.filter.session_id?'会话缓存趋势':'缓存命中概览'}</h2><p>本页 ${items.length} 条 · 按请求顺序，左旧右新 · 点击节点查看详情</p></div><span class="cache-scope">${icon('requests')}当前页</span></header><div class="cache-summary"><div class="cache-summary-primary"><span>加权命中率</span><strong>${cachePercent(s.rate)}</strong><small>缓存输入总量 ÷ 输入总量</small></div><div><span>最低命中率</span><strong>${cachePercent(s.minimum)}</strong><small>${s.measured} 条有效用量 · ${s.unknown} 条未知</small></div><button class="cache-drop-filter ${state.cacheDropsOnly?'active':''}" data-action="cache-drops" aria-pressed="${!!state.cacheDropsOnly}"><span>${icon('drop')}明显下降</span><strong>${s.drops}<small> 次</small></strong><small>${state.cacheDropsOnly?'显示全部本页请求':'仅看本页下降请求'} ${icon('arrow')}</small></button></div><div class="cache-plot"><svg viewBox="0 0 960 138" role="group" aria-label="当前页请求缓存命中率趋势；同 Key、会话、线程的可比较请求连线">${[0,50,100].map(v=>`<line class="cache-gridline" x1="${left}" x2="${right}" y1="${bottom-v*(bottom-top)/100}" y2="${bottom-v*(bottom-top)/100}"/><text x="0" y="${bottom-v*(bottom-top)/100+4}" class="cache-axis">${v}%</text>`).join('')}${lines}${dots}</svg></div><div class="cache-chart-foot"><span>${items.length?dt(ordered[0].created_at):'—'}</span><span><i></i>命中率 <i class="is-drop"></i>下降 ≥ ${CacheMetrics.dropThreshold} 个百分点 ${points.some(p=>p.m.rate==null)?'<i class="is-unknown"></i>底部灰点：未知 / 不适用':''}</span><span>${items.length?dt(ordered.at(-1).created_at):'—'}</span></div><p class="cache-chart-note">仅连接同 Key、会话、线程中模型与绑定一致的相邻调用。前次基准跨分页查找；图表与汇总仅覆盖本页。</p></section>`;
}
function requestRows(items){return items.map(r=>{
 const search=r.kind==='search',u=r.usage||{},m=CacheMetrics.compare(r),factor=billingFactor(r),fast=['fast','priority'].includes(u.service_tier),tps=tokensPerSecond(r),first=firstTokenMs(r);
 return `<tr class="clickable ${m.drop?'cache-drop-row':''}" data-action="request-detail" data-id="${r.id}"><td class="request-time"><time datetime="${esc(r.created_at)}">${dt(r.created_at)}</time><button class="trace-request-id ghost" data-action="request-detail" data-id="${r.id}" title="${r.id}">${icon('requests')}${short(r.id)}${icon('arrow')}</button>${r.client_session_id?`<button class="trace-session-link ghost" data-action="request-session" data-session="${esc(r.client_session_id)}" data-key="${r.key_id}" title="${esc(r.client_session_id)}">${icon('activity')}此会话</button>`:''}</td><td class="request-model"><div class="trace-source-line">${requestType(r)}${clientSource(r)}</div><div class="mono name">${esc(r.model)}</div><span class="cache-note">${search?'按成功次数计费':'思考 · '+reasoningLabel(r.reasoning_effort)}</span></td><td class="request-cache">${cacheCell(r)}</td><td class="request-tokens"><div class="trace-stack">${search?traceChip('成功',n(u.search_calls)+' 次','check'):traceChip('输入',u.input_tokens==null?'未知':n(u.input_tokens),'download')+traceChip('输出',u.output_tokens==null?'未知':n(u.output_tokens),'arrow')}</div>${!search?`<span class="cache-note">缓存输入 ${u.cached_input_tokens==null?'未知':n(u.cached_input_tokens)}</span>`:''}</td><td class="request-performance"><span class="request-cost trace-chip" data-billing tabindex="0" aria-label="查看计费详情" aria-describedby="billing-tooltip">${icon('wallet')}<b>${r.valuation?.cny==null?'未计价':'¥ '+money(r.valuation.cny)}</b>${!search&&fast&&factor!=null?`<span class="fast-badge" aria-label="Fast 计费倍率 ${esc(Number(factor))}">×${esc(Number(factor))}</span>`:''}<template>${billingTooltip(r)}</template></span><div class="trace-latency">${icon('clock')}<span>总计 <b>${ms(r.total_ms)}</b></span></div><span class="cache-note" title="首字包含排队；TPS = 输出 Token ÷ 总耗时">${search?'Search':'首字 '+ms(first)+' · '+(tps==null?'—':tps.toLocaleString('zh-CN',{maximumFractionDigits:1}))+' t/s'}</span></td><td class="request-account">${tag(r.state)}<span class="account-name" title="${esc(r.account_name||'')}">${icon('accounts')}${esc(r.account_name||(r.account_id?'账户不可用':'—'))}</span></td></tr>`;
});}
async function requestsPage(api){
 const p=new URLSearchParams({...state.filter,offset:state.offset,limit:25}),d=await api('/requests?'+p),visible=state.cacheDropsOnly?d.items.filter(r=>CacheMetrics.compare(r).drop):d.items;
 return head('请求追踪','查看缓存复用，定位每一次调用。')+`<form class="filters trace-filters" data-form="filter"><label>会话 ID<input name="session_id" class="wide" value="${esc(state.filter.session_id)}" placeholder="客户端 session_id"></label><label>客户端来源<select name="client_source"><option value="">全部来源</option><option value="codex" ${state.filter.client_source==='codex'?'selected':''}>Codex</option><option value="unknown" ${state.filter.client_source==='unknown'?'selected':''}>未知来源</option></select></label><label>接口<select name="kind"><option value="">全部接口</option><option value="responses" ${state.filter.kind==='responses'?'selected':''}>Responses</option><option value="compact" ${state.filter.kind==='compact'?'selected':''}>Compact</option><option value="search" ${state.filter.kind==='search'?'selected':''}>Search</option></select></label><label>状态<select name="state"><option value="">全部状态</option>${['queued','inflight','completed','failed','cancelled','interrupted','rejected'].map(x=>`<option ${state.filter.state===x?'selected':''} value="${x}">${label(x)}</option>`).join('')}</select></label><label>模型<input name="model" value="${esc(state.filter.model)}" placeholder="模型名称"></label><label>请求 / 账户 / Key ID<input name="lookup" class="wide" value="${esc(state.filter.id||state.filter.account_id||state.filter.key_id)}" placeholder="完整 UUID"></label><label>查找类型<select name="lookup_kind">${[['id','请求'],['account_id','账户'],['key_id','Key']].map(([k,v])=>`<option value="${k}" ${state.filter[k]?'selected':''}>${v}</option>`).join('')}</select></label><button class="primary" type="submit">筛选</button><button type="button" data-action="filter-reset" class="ghost">重置</button></form>${state.filter.session_id?`<div class="trace-session-scope">${icon('activity')}<span>会话</span><code>${esc(state.filter.session_id)}</code>${state.filter.key_id?`<small>Key ${short(state.filter.key_id)}</small>`:''}</div>`:''}${d.items.some(r=>r.kind!=='search')?cacheOverview(d.items):''}<section class="panel request-panel"><div class="trace-list-head"><h2>${state.cacheDropsOnly?'明显下降 · 本页':'请求记录'}</h2><span>${icon('requests')}${n(d.total)} 条匹配 · 最新在前</span>${state.cacheDropsOnly?'<button class="small ghost" data-action="cache-drops">显示全部</button>':''}</div>${visible.length?table(['请求','接口与模型','缓存命中率','用量','费用与耗时','状态与账户'],requestRows(visible)):empty(state.cacheDropsOnly?'本页没有明显下降':'没有匹配的请求',state.cacheDropsOnly?'可切换其他页，或显示全部本页请求。':'调整筛选条件，或通过 API 发起请求。')}</section><div class="pagination"><span>共 ${n(d.total)} 条 · 第 ${Math.floor(state.offset/25)+1} 页</span><div><button class="small" data-action="request-prev" ${state.offset?'':'disabled'}>上一页</button><button class="small" data-action="request-next" ${state.offset+25<d.total?'':'disabled'}>下一页</button></div></div>`;
}
async function modelsPage(api){
 const [m,p,d,j,sp]=await Promise.all([api('/models'),api('/prices'),api('/models/discovered'),api('/models/sync'),api('/search-price')]);state.searchPrice=sp;state.models=m.items;state.prices=p.items;state.discovered=d.items;state.modelSyncRunning=j.running;
 const rows=m.items.map(m=>{const p=state.prices.find(p=>sameModel(p.model,m.upstream)),source=d.items.find(x=>sameModel(x.model,m.upstream));return `<tr><td><div class="mono name">${esc(m.id)}</div><div class="sub">上游 ${esc(m.upstream.model)}</div></td><td>${tag(m.enabled?'enabled':'disabled')}</td><td>${source?source.accounts.length+' 个账户':'手动配置'}<div class="sub" title="${esc(source?.accounts.map(a=>a.name).join('、')||'')}">${esc(source?.accounts.map(a=>a.name).join('、')||'尚未获取上游目录')}</div></td><td>${p?`${money(p.standard.input_per_million)} / ${money(p.standard.cached_input_per_million)} / ${money(p.standard.output_per_million)}`:'待填价格'}${p?'':'<div class="sub">选择模型后填写单价即可</div>'}</td><td>${p?.fast_multiplier?'× '+esc(p.fast_multiplier):'待设置'}</td><td><div class="actions"><button class="small" data-action="price-edit" data-id="${esc(m.id)}">${p?'修改价格':'填写价格'}</button><button class="small ghost" data-action="model-edit" data-id="${esc(m.id)}">编辑</button></div></td></tr>`;});
 const failures=d.accounts.filter(a=>a.error);const fetched=d.accounts.filter(a=>a.synced_at).length;
 return head('模型与价格','汇总所有账户的上游模型，填写标准价格；Fast 按倍率计算。',`<button data-action="model-edit">手动添加</button><button data-action="price-new" ${m.items.length?'':'disabled'}>快速填价</button><button class="primary" data-action="models-sync" ${j.running?'disabled':''}>${j.running?'获取中 '+j.completed+' / '+j.total:'从所有账户获取模型'}</button>`)+`<section class="search-price-bar"><div><h2>Search 按次计费</h2><p>所有账户与模型共用 · 成功请求计 1 次 · 不叠加 Token 费用或 Fast 倍率</p></div><div class="search-price-value">${sp.per_call==null?'未设置':'¥ '+money(sp.per_call)+' / 次'}</div><button data-action="search-price-edit">设置单价</button></section><div class="notice">${fetched} / ${d.accounts.length} 个账户已有上游目录 · 发现 ${d.items.length} 个模型${j.running?' · 正在获取，完成后自动更新列表':''}${failures.length?`<details><summary>${failures.length} 个账户最近获取失败，已保留之前的模型列表</summary>${failures.map(a=>`<p>${esc(a.name)}：${esc(a.error.message)}</p>`).join('')}</details>`:''}</div><section class="panel">${rows.length?table(['模型','状态','支持账户','输入 / 缓存 / 输出（¥ / 百万 Token）','Fast 倍率','操作'],rows):empty('从账户获取模型名称','先添加账户，再获取上游目录。无需逐个输入模型名称。','<button class="primary" data-action="models-sync">获取上游模型</button>')}</section><p class="muted">模型名称在账户间去重；标准价格与 Fast 倍率按模型保存，历史请求费用保持不变。</p>`;
}
function sameModel(a,b){return a.provider===b.provider&&a.access_kind===b.access_kind&&a.model===b.model;}
async function loadGroups(read=api){const d=await read('/groups');state.groups=d.items;state.defaultGroup=d.default_group_id;return d;}
function groupName(id){return state.groups.find(g=>g.id===id)?.name||'未知分组';}
function groupChips(ids){return ids?.length?ids.map(id=>`<span class="group-chip">${icon('groups')}<span>${esc(groupName(id))}</span></span>`).join(''):'<span class="group-chip muted">未分组</span>';}
function groupOptions(selected){return state.groups.map(g=>`<option value="${g.id}" ${selected===g.id?'selected':''}>${esc(g.name)}</option>`).join('');}
function groupFilter(id,selected){return `<label class="group-filter">${icon('groups')}<select id="${id}" aria-label="筛选分组"><option value="">全部分组</option>${groupOptions(selected)}</select></label>`;}
function groupSelection(ids){const selected=ids??[state.defaultGroup];return `<div class="field full"><span>所属分组</span><div class="group-selection">${state.groups.map(g=>`<label class="group-option"><input type="checkbox" name="group_id" value="${g.id}" ${selected.includes(g.id)?'checked':''}>${icon('groups')}<span>${esc(g.name)}</span></label>`).join('')}</div></div>`;}
async function groupsPage(api){
 await loadGroups(api);
 return `<div class="groups-workspace">${head('分组','',`<button class="primary" data-action="group-create">${icon('plus')}创建分组</button>`)}<section class="panel groups-panel">${table(['分组名称','账户','API Keys','操作'],state.groups.map(g=>`<tr><td><div class="group-name">${icon('groups')}<strong>${esc(g.name)}</strong>${g.is_default?'<span class="tag">默认</span>':''}</div></td><td><div class="group-counts"><span class="account-count">${icon('accounts')}${n(g.account_count)} 个账户</span><span class="account-count ${g.enabled_account_count?'enabled-count':'empty-count'}">${icon('activity')}${n(g.enabled_account_count)} 已启用</span></div></td><td><span class="account-count">${icon('keys')}${n(g.key_count)} 个 Key</span></td><td><div class="actions"><button class="small" data-action="group-accounts" data-id="${g.id}">${icon('accounts')}账户</button><button class="small icon ghost" data-action="group-edit" data-id="${g.id}" aria-label="重命名分组 ${esc(g.name)}">${icon('edit')}</button><button class="small icon ghost danger" data-action="group-delete" data-id="${g.id}" aria-label="删除分组 ${esc(g.name)}" title="${g.is_default?'默认分组不可删除':g.account_count||g.key_count?'先移出组内账户和 Key':'删除空分组'}" ${g.is_default||g.account_count||g.key_count?'disabled':''}>${icon('trash')}</button></div></td></tr>`))}</section></div>`;
}
function groupEdit(id){const g=state.groups.find(g=>g.id===id);modal(g?'重命名分组':'创建分组',`<form data-form="group" data-id="${id||''}">${field('分组名称','name',g?.name||'','text','','required maxlength="128"')}${formActions(g?'保存':'创建分组')}</form>`);}
async function keyEdit(id){await loadGroups();const k=state.keys.find(k=>k.id===id);modal(k?'编辑 Key':'创建 API Key',`<form data-form="${k?'key-edit':'key'}" data-id="${id||''}"><div class="form-grid">${field('名称','name',k?.name||'','text','','required maxlength="128"')}<label class="field">所属分组<select name="group_id" required>${groupOptions(k?.group_id||state.keyGroupFilter||state.defaultGroup)}</select></label></div>${formActions(k?'保存':'创建 Key')}</form>`);}
async function keysPage(api){
 const [d]=await Promise.all([api('/keys'),loadGroups(api)]);state.keys=d.items;
 const keys=d.items.filter(k=>!state.keyGroupFilter||k.group_id===state.keyGroupFilter);
 return `<div class="keys-workspace">${head('API Keys','',`<button class="primary" data-action="key-create">${icon('plus')}创建 Key</button>`)}<div class="accounts-summary">${groupFilter('key-group-filter',state.keyGroupFilter)}<span class="account-count">${icon('keys')}${n(keys.length)} 个 Key</span></div><section class="panel">${keys.length?table(['名称','所属分组','密钥前缀','状态','最近使用','操作'],keys.map(k=>`<tr><td class="name">${esc(k.name)}</td><td>${groupChips([k.group_id])}</td><td><span class="key-prefix">${icon('keys')}<code>${esc(k.prefix)}…</code></span></td><td><div class="account-toggle"><button class="toggle-switch" role="switch" aria-checked="${k.enabled}" aria-label="启用 Key ${esc(k.name)}" data-action="key-enabled" data-id="${k.id}" data-enabled="${!k.enabled}"><span></span></button><span>${k.enabled?'已启用':'已停用'}</span></div></td><td>${dt(k.last_used_at)}</td><td><button class="small icon ghost" data-action="key-edit" data-id="${k.id}" aria-label="编辑 Key ${esc(k.name)}">${icon('edit')}</button><button class="small icon ghost danger" data-action="key-delete" data-id="${k.id}" aria-label="删除 Key ${esc(k.name)}" title="删除 Key">${icon('trash')}</button></td></tr>`)):empty('暂无 Key','','<button class="primary" data-action="key-create">创建 Key</button>')}</section></div>`;
}
const settingGroups=[['等待与超时','仅并发满载时排队，无可用账户立即拒绝。',[['queue_timeout_ms','最长排队时间','毫秒'],['heartbeat_interval_ms','排队心跳间隔','毫秒'],['connect_timeout_ms','上游连接超时','毫秒'],['sse_idle_timeout_ms','上游 SSE 空闲超时','毫秒']]],['并发与容量','同一 session 的不同线程可并行，同一线程保持串行；同时受账户和全局上限约束。下调只限制新增占用。',[['global_max_inflight','全局并发上限','请求'],['session_max_inflight','单 session 并发上限','请求 · 1–1000，默认 2'],['default_account_concurrency','新账户默认并发','请求'],['queue_capacity','队列容量','请求'],['queue_memory_bytes','内存预算','字节'],['request_body_limit_bytes','单请求正文上限','字节'],['sse_event_limit_bytes','单 SSE 事件上限','字节']]],['额度采集','过期额度展示为未知，额度恢复不自动启用账户。',[['quota_poll_interval_secs','后台采集间隔','秒'],['quota_stale_after_secs','额度数据有效期','秒']]],['数据保留','修改保留期限后自动启动清理，小时汇总长期保留。',[['request_retention_days','请求明细保留','天'],['audit_retention_days','管理员审计保留','天']]]];
async function settingsPage(api){
 const [c,cleanup]=await Promise.all([api('/settings'),api('/cleanup')]);state.config=c;
 return head('运行配置',`当前版本 v${c.version} · 保存成功即表示新版本已生效。`)+`<form class="settings" data-form="settings">${settingGroups.map(([title,desc,fields])=>`<section class="settings-group"><div><h3>${title}</h3><p>${desc}</p></div><div class="form-grid">${fields.map(([k,l,u])=>field(l,k,c[k],'number',u,`required min="1" step="1"${k==='session_max_inflight'?' max="1000"':''}`)).join('')}</div></section>`).join('')}<div class="notice">内存预算覆盖接收中的请求、处理中的请求和流式缓冲；请求正文按保守倍率预留解析与复制空间。</div><div class="form-actions"><span class="text-error" role="alert"></span><span class="muted">版本 v${c.version}</span><button class="primary" type="submit">保存并立即生效</button></div></form><section class="detail-section"><div class="actions"><h3>数据清理</h3><span class="tag">${esc(label(cleanup.state))}</span><button class="small" data-action="cleanup">立即执行</button></div><p class="muted">最近完成：${dt(cleanup.finished_at)} · 删除请求 ${n(cleanup.requests_deleted)} 条</p></section>`;
}
async function usagePage(api){
 const filter=state.usageFilter||{};
 const [d,a,k,m]=await Promise.all([api('/dashboard?'+new URLSearchParams(filter)),api('/accounts'),api('/keys'),api('/models')]);
 const options=(items,key,name,selected)=>items.map(x=>`<option value="${esc(x[key])}" ${selected===x[key]?'selected':''}>${esc(x[name])}</option>`).join('');
 const datetime=x=>x?new Date(new Date(x).getTime()-new Date(x).getTimezoneOffset()*60000).toISOString().slice(0,16):'';
 const s=d.summary;
 return head('用量费用','按账户、Key、模型、实际档位与时间范围汇总；时间边界使用整点。')+`<form class="filters" data-form="usage-filter"><label>账户<select name="account_id"><option value="">全部账户</option>${options(a.items.map(x=>x.account),'id','name',filter.account_id)}</select></label><label>API Key<select name="key_id"><option value="">全部 Key</option>${options(k.items,'id','name',filter.key_id)}</select></label><label>模型<select name="model"><option value="">全部模型</option>${options(m.items,'id','id',filter.model)}</select></label><label>实际档位<select name="service_tier"><option value="">全部档位</option>${['default','priority','fast','unknown'].map(x=>`<option value="${x}" ${filter.service_tier===x?'selected':''}>${x}</option>`).join('')}</select></label><label>开始（含）<input type="datetime-local" step="3600" name="from" value="${datetime(filter.from)}"></label><label>结束（不含）<input type="datetime-local" step="3600" name="to" value="${datetime(filter.to)}"></label><button type="submit">查询</button><button type="button" class="ghost" data-action="usage-reset">重置</button></form><section class="metrics"><div class="metric"><label>请求数量</label><strong>${n(s.requests)}</strong><small>${n(s.completed)} 完成 · ${n(s.failed)} 失败</small></div><div class="metric"><label>输入 Token</label><strong>${n(s.input_tokens)}</strong><small>其中缓存 ${n(s.cached_tokens)}</small></div><div class="metric"><label>输出 Token</label><strong>${n(s.output_tokens)}</strong><small>图片生成 ${n(s.images)} 张</small></div><div class="metric"><label>已计价费用</label><strong><em>¥</em>${money(s.cny)}</strong><small>${n(s.unpriced)} 次未完整计价</small></div></section><div class="search-usage-summary"><span>Search 成功调用 <strong>${n(s.search_calls)} 次</strong></span><span>Search 已计价费用 <strong>¥ ${money(s.search_cny)}</strong></span></div><section class="panel"><div class="panel-head"><h2>模型汇总</h2><span class="muted">${n(s.incomplete_usage)} 次用量不完整</span></div>${d.models.length?table(['模型','请求数量','Search 次数','Search 费用','已计价总费用'],d.models.map(x=>`<tr><td class="mono">${esc(x.model)}</td><td>${n(x.requests)}</td><td>${n(x.search_calls)}</td><td>¥ ${money(x.search_cny)}</td><td>¥ ${money(x.cny)}</td></tr>`)):empty('当前范围没有用量','调整筛选条件后重新查询。')}</section>`;
}
async function auditPage(api){
 const d=await api(`/audit?offset=${state.auditOffset}&limit=30`);return head('操作审计','查看管理员操作、账户变化与运行配置历史。')+`<section class="panel">${d.items.length?table(['时间','操作','来源','账户','详情'],d.items.map(e=>`<tr><td>${dt(e.at)}</td><td>${esc(label(e.kind))}</td><td>${esc(e.actor)}</td><td class="mono">${short(e.account_id)}</td><td><details><summary>展开详情</summary><pre>${esc(JSON.stringify(e.details,null,2))}</pre></details></td></tr>`)):empty('暂无操作记录','管理员和系统操作将自动记录。')}</section><div class="pagination"><span>共 ${n(d.total)} 条</span><div><button class="small" data-action="audit-prev" ${state.auditOffset?'':'disabled'}>上一页</button><button class="small" data-action="audit-next" ${state.auditOffset+30<d.total?'':'disabled'}>下一页</button></div></div>`;
}
const rewriteLabels={unchanged:'保持不变',rewritten:'归一化',added:'自动补充',removed:'未转发',alias_restored:'还原旧别名'};
function requestRewrites(d){
 const trace=d.events.findLast(e=>e.kind==='request_rewritten')?.details;
 if(!trace)return `<section class="detail-section"><h3>本次请求改写</h3><p class="muted">${d.request.upstream_attempts?'该历史请求没有逐字段改写记录。此功能启用后的新请求会显示完整对照。':'该请求没有转发改写记录。'}</p></section>`;
 const entries=trace.entries||[],unchanged=entries.filter(e=>e.action==='unchanged').length,changed=entries.length-unchanged;
 const rows=entries.map(e=>`<tr data-rewrite-action="${esc(e.action)}" ${e.action==='unchanged'?'hidden':''}><td class="rewrite-field">${esc(e.field)}</td><td class="mono rewrite-value">${e.before==null?'<span class="muted">未提供</span>':esc(e.before)}</td><td class="mono rewrite-value">${e.after==null?'<span class="muted">未转发</span>':esc(e.after)}</td><td><span class="rewrite-action ${e.action==='unchanged'?'':'changed'}">${esc(rewriteLabels[e.action]||e.action)}</span></td></tr>`);
 return `<section class="detail-section request-rewrites"><h3>本次请求改写</h3><p class="muted">对照传入请求与转发请求中的标识字段。headers 为请求头，body 为请求体；对象 ID 随内容透传。</p><div class="rewrite-summary"><strong>${changed} 项变更</strong><span>${unchanged} 项保持不变</span></div><div class="rewrite-controls"><input id="rewrite-search" aria-label="搜索改写字段或 ID" placeholder="搜索字段或 ID"><label class="check"><input id="rewrite-unchanged" type="checkbox">显示未改写 ID</label></div>${trace.omitted?`<p class="notice">有 ${n(trace.omitted)} 项因数量或 ID 长度限制未记录。最多记录 2,000 项，每个 ID 不超过 512 字节。</p>`:''}<div class="rewrite-table">${table(['字段位置','传入 ID','转发 ID','处理'],rows)}</div><p id="rewrite-empty" class="muted" ${changed?'hidden':''}>没有匹配的变更，可勾选「显示未改写 ID」。</p></section>`;
}
function filterRewrites(){
 const search=$('#rewrite-search')?.value.trim().toLowerCase()||'',all=$('#rewrite-unchanged')?.checked;let visible=0;
 document.querySelectorAll('[data-rewrite-action]').forEach(row=>{row.hidden=(!all&&row.dataset.rewriteAction==='unchanged')||!row.textContent.toLowerCase().includes(search);if(!row.hidden)visible++;});
 if($('#rewrite-empty'))$('#rewrite-empty').hidden=visible>0;
}
document.addEventListener('input',event=>{if(event.target.id==='rewrite-search')filterRewrites();});
function timelineDetails(e){
 if(e.kind==='request_rewritten')return `记录 ${e.details.entries?.length||0} 项标识对照${e.details.omitted?'，另有 '+e.details.omitted+' 项未记录':''}，见「本次请求改写」。`;
 if(e.kind==='upstream_attempt_headers'&&e.details.turn_state){const {turn_state,...details}=e.details;return JSON.stringify({...details,turn_state:turn_state.present?'已返回，见「Turn State 观察」':'未返回'},null,2);}
 return JSON.stringify(e.details,null,2);
}
function turnStateValue(observation){
 if(!observation)return '<p class="muted">未采集 · 历史记录无法判断</p>';
 if(!observation.present)return '<p class="muted">未携带该字段</p>';
 const values=observation.values||[];
 return `<p class="muted">${n(observation.total_values)} 个值${observation.omitted_values?' · '+n(observation.omitted_values)+' 个因采集上限省略':''}</p>${values.map((v,i)=>`<details><summary>值 ${i+1} · ${n(v.bytes)} 字节${v.encoding==='base64'?' · 非 UTF-8，以 Base64 保存':''}${v.truncated?' · 已截断':''}${v.bytes===0?' · 空字符串':''}</summary><pre class="turn-state-value">${v.bytes===0?'（空字符串）':esc(v.value)}</pre></details>`).join('')}`;
}
function turnStateDetail(d){
 const headers=d.events.filter(e=>e.kind==='upstream_attempt_headers'),r=d.request;
 const upstream=headers.length?headers.map(e=>`<div class="turn-state-attempt"><h4>上游第 ${n(e.details.attempt)} 次 · HTTP ${esc(e.details.status)}</h4>${turnStateValue(e.details.turn_state)}</div>`).join(''):`<p class="muted">${!r.client_turn_state?'历史记录未采集':r.upstream_attempts?'尚未记录上游响应头':'尚未调用上游'}</p>`;
 return `<section class="detail-section turn-state-detail"><h3>Turn State 观察</h3><p class="muted"><code>x-codex-turn-state</code> · 客户端传入值仍被丢弃，不向上游转发。</p><h4>客户端传入</h4>${turnStateValue(r.client_turn_state)}${upstream}${headers.length<r.upstream_attempts?'<p class="muted">部分尝试未记录响应头，请结合最终错误查看。</p>':''}</section>`;
}
function encryptedReasoningRecovery(d){
 const recovery=d.events.find(e=>e.kind==='encrypted_reasoning_recovery')?.details;
 if(!recovery)return '';
 const r=d.request,c=recovery.cleanup||{},sent=r.upstream_attempts>1;
 const outcome=!sent?'未补发':r.state==='completed'?'恢复成功':r.state==='inflight'?'补发处理中':'补发未完成 · '+label(r.state);
 const attempts=d.events.filter(e=>e.kind==='upstream_attempt_headers').map(e=>e.details);
 return `<section class="detail-section encrypted-reasoning-recovery"><h3>加密推理恢复</h3>${fact([['触发原因','HTTP 400 · invalid_encrypted_content'],['恢复结果',esc(outcome)],['移除密文字段',n(c.encrypted_fields_removed)+' 项'],['移除空 content / 空 reasoning 条目',n(c.null_content_fields_removed)+' / '+n(c.empty_reasoning_items_removed)],['上游调用次数',n(r.upstream_attempts)]])}<p class="muted">在原账户和绑定上最多补发一次。移除的加密推理状态不再参与本次上下文；保留摘要和其余历史内容。</p>${attempts.length?table(['上游尝试','HTTP 状态','响应头耗时','上游请求 ID'],attempts.map(a=>`<tr><td>第 ${n(a.attempt)} 次</td><td>${esc(a.status)}</td><td>${ms(a.headers_ms)}</td><td class="mono">${esc(a.upstream_request_id||'未提供')}</td></tr>`)):''}<p class="muted">未收到响应头的尝试请结合最终错误和请求时间线查看。</p></section>`;
}
function ingressDiagnostics(r){
 const d=r.ingress_diagnostics;if(!d)return '';
 const identity=d.identity||{},resolved=identity.resolved||{};
 const stages={authentication:'网关 Key 鉴权',body_encoding:'请求编码校验',body_read:'读取请求体',json_parse:'JSON 解析',ingress_validation:'协议与身份校验',admission:'来源、模型校验与入队',queue_or_prepare:'排队与派发准备',upstream_transport:'连接或发送上游',upstream_response:'读取或处理上游响应'};
 const statuses={valid:'有效',empty:'空值',too_long:'过长，未保存值',invalid_characters:'字符无效，未保存值',invalid_type:'类型无效',invalid_encoding:'编码无效'};
 const fields=identity.fields||[],conflicts=new Set(fields.filter(f=>new Set(fields.filter(x=>x.field===f.field&&x.status==='valid').map(x=>x.value)).size>1).map(f=>f.field));
 const rows=fields.map(f=>{const roles=[];if(resolved.session_sources?.includes(f.source))roles.push('会话');if(resolved.thread_sources?.includes(f.source))roles.push('线程');return `<tr><td class="mono">${esc(f.source)}</td><td class="mono">${esc(f.value??'—')}</td><td>${esc(conflicts.has(f.field)?'值冲突':statuses[f.status]||f.status)}${roles.length?' · 采用为'+roles.join('、'):''}</td></tr>`;});
 return `<section class="detail-section ingress-diagnostics"><h3>请求接入诊断</h3>${fact([['请求接口',esc(d.method+' '+d.path)],['客户端来源',clientSource({client_origin:d.client_origin})],['失败阶段',esc(stages[d.failure_stage]||d.failure_stage||'—')],['请求体',n(d.body_bytes)+' 字节 · '+esc({parsed:'已解析',not_parsed:'未解析',invalid_json:'JSON 无效'}[d.body_status]||d.body_status)],['授权头',d.authorization_present?'存在（不记录凭证）':'未提供'],['会话来源',esc(resolved.session_sources?.join('、')||(identity.status==='stateless'?'未提供 · 独立请求':'未解析'))],['线程来源',esc(resolved.thread_sources?.map(x=>x==='resolved.session_id'?'沿用会话 ID':x).join('、')||'未解析')]])}${identity.error?`<div class="notice error">${esc(identity.error.message)}</div>`:''}${rows.length?table(['收到的身份字段','值','解析状态'],rows):'<p class="muted">未观察到可识别的会话或线程字段。</p>'}<details><summary>完整诊断信息</summary><pre>${esc(JSON.stringify(d,null,2))}</pre></details></section>`;
}
function compactionDetail(r){
 const c=r.compaction;if(!c)return '';
 const method={compact:'独立 Compact',remote_v2:'Remote V2'}[c.method]||'未知';
 const observed=c.output_observed===true?'已返回':c.output_observed===false?'未返回':'尚未确认';
 return `<section class="detail-section compaction-detail"><h3>上下文压缩</h3>${fact([['操作类型','上下文压缩'],['压缩方式',method],['执行结果',esc(label(r.state))],['压缩项',observed]])}<p class="muted">用量与费用按上游实际返回统计。客户端负责替换上下文；网关不保存压缩正文。</p></section>`;
}
function modelRoutingDetail(r){
 if(r.kind==='search')return '';
 const known=v=>typeof v==='string'&&v.length>0;
 const returned=known(r.response_model),sent=known(r.upstream_model),routed=returned&&sent&&r.response_model!==r.upstream_model;
 const status=routed?'<span class="tag wait">模型存在路由</span>':returned&&sent?'<span class="tag good">模型一致</span>':'<span class="tag">无法判断</span>';
 const hint=routed?'上游返回的模型名称与发往上游的模型不同；此标记依据响应字段判断。':returned&&sent?'上游返回的模型名称与发往上游的模型一致。':!returned?'未记录上游返回模型，无法判断是否存在路由。':'未记录发往上游的模型，无法进行比较。';
 return `<section class="detail-section model-routing"><h3>模型路由 ${status}</h3>${fact([['客户端请求模型',esc(r.model||'未记录')],['发往上游模型',esc(r.upstream_model||'未记录')],['上游返回模型',esc(r.response_model||'未记录')]])}<p class="muted">${hint}</p></section>`;
}
async function requestDetail(id){
 modal('请求详情','<div class="notice" role="status">正在加载请求详情…</div>',{subtitle:id,drawer:true});
 const version=dialogVersion,controller=new AbortController();detailLoad=controller;
 try{
 const read=(path)=>api(path,'GET',undefined,controller.signal);
 const [d]=await Promise.all([read('/requests/'+id),loadGroups(read)]);
 if(!dialog.open||version!==dialogVersion)return;
 const r=d.request,u=r.usage,v=r.valuation;
 modal('请求详情',`${tag(r.state)}${modelRoutingDetail(r)}${r.kind==='search'?'':cacheDetail(r)}${fact([['请求 ID',`<span class="mono">${esc(r.id)}</span>`],['接口类型',requestType(r)],['客户端来源',clientSource(r)],['识别依据',esc(r.client_origin?.evidence?.join('、')||'未记录或未匹配')],['模型 / 请求档位',`${esc(r.model)} / ${esc(r.requested_tier||'默认')}`],['思考等级',reasoningLabel(r.reasoning_effort)],['开始时间',dt(r.created_at)],['Key ID',`<span class="mono">${esc(r.key_id)}</span>`],['请求分组',r.group_id?esc(groupName(r.group_id))+' · '+esc(r.group_id):'—'],['客户端会话',r.stateless?'未提供 · 独立请求':`<span class="mono">${esc(r.client_session_id)}</span>`],['客户端线程',`<span class="mono">${esc(r.client_thread_id)}</span>`],['账户 / 绑定代次',`${esc(r.account_id||'—')} / ${r.binding_generation||'—'}`],['上游请求 ID',esc(r.upstream_request_id||'—')],['上游模型 / 响应头耗时',`${esc(r.upstream_model||'—')} / ${ms(r.upstream_headers_ms)}`],['上游 HTTP / 调用次数',`${r.upstream_status||'—'} / ${r.upstream_attempts}`],['排队 / 首事件 / 首内容',`${ms(r.queue_ms)} / ${ms(r.first_event_ms)} / ${ms(r.first_content_ms)}`],['总耗时 / 配置版本',`${ms(r.total_ms)} / ${r.config_versions.map(v=>'v'+v).join(' → ')}`]])}${compactionDetail(r)}${turnStateDetail(d)}${encryptedReasoningRecovery(d)}${r.error_code?`<div class="notice error"><strong>${esc(r.error_code)}</strong><br>${esc(r.error_message)}${r.upstream_error?`<p>${esc(ErrorCenter.cause(r.upstream_error))}</p>`:''}<button class="small ghost" data-action="errors-related" data-code="${esc(r.error_code)}" data-at="${esc(r.created_at)}">查看同类错误 ${icon('arrow')}</button></div>`:''}${ingressDiagnostics(r)}<section class="detail-section"><h3>用量与计价</h3>${r.kind==='search'?fact([['Search 成功次数',n(u.search_calls)],['全局单价',r.search_price?.per_call==null?'未设置':'¥ '+money(r.search_price.per_call)+' / 次'],['金额',v?.cny==null?'未计价':'¥ '+money(v.cny)],['计价状态',esc(label(v?.status))]]):fact([['输入 / 缓存输入',`${u.input_tokens==null?'未知':n(u.input_tokens)} / ${u.cached_input_tokens==null?'未知':n(u.cached_input_tokens)}`],['输出 / 推理输出',`${u.output_tokens==null?'未知':n(u.output_tokens)} / ${u.reasoning_output_tokens==null?'未知':n(u.reasoning_output_tokens)}`],['实际档位 / 完整用量',`${esc(u.service_tier||'未知')} / ${u.complete?'是':'否'}`],['金额',v?.cny==null?'未完整计价':'¥ '+money(v.cny)],['计价状态',esc(label(v?.status))]])}${v?.items?.length?table(['计量项','数量','金额'],v.items.map(i=>`<tr><td>${esc(i.kind)}</td><td>${n(i.quantity)}</td><td>¥ ${money(i.cny)}</td></tr>`)):''}</section>${requestRewrites(d)}<section class="detail-section"><h3>请求时间线</h3><ol class="timeline">${d.events.map(e=>`<li><strong>${esc(label(e.kind))}</strong><time>${dt(e.at)}</time><pre>${esc(timelineDetails(e))}</pre></li>`).join('')}</ol></section><section class="detail-section"><h3>历史绑定与累计映射</h3><details><summary>${d.bindings.length} 个绑定代次 · ${d.mappings.length} 条 ID 映射</summary><pre>${esc(JSON.stringify({bindings:d.bindings,mappings:d.mappings},null,2))}</pre></details></section>`,{subtitle:r.id,drawer:true});
 }catch(e){if(e.name!=='AbortError'&&dialog.open&&version===dialogVersion)modal('请求详情',`<div class="notice error" role="alert">${esc(e.message)}</div><button data-action="request-detail" data-id="${esc(id)}">重试</button>`,{subtitle:id,drawer:true});}
}
async function accountDetail(id){await loadGroups();const d=await api('/accounts/'+id),a=d.account,s=d.statistics.summary;modal(a.name,`${accountSwitch(a)}${fact([['邮箱',esc(d.email||'未知')],['账户 ID',esc(a.id)],['所属分组',groupChips(a.group_ids)],['客户端来源限制',a.codex_only?'仅允许 Codex':'允许所有来源（含未知）'],['上游账户',esc(a.upstream_account_id)],['并发上限',a.max_inflight],['启停原因',esc(label(a.disable_reason))],['授权到期',dt(a.credential_expires_at)],['累计请求',n(s.requests)],['输入 / 输出 Token',`${n(s.input_tokens)} / ${n(s.output_tokens)}`],['累计已计价金额','¥ '+money(s.cny)],['Search 成功次数',n(s.search_calls)],['Search 已计价金额','¥ '+money(s.search_cny)],['本周期 5h 金额',spendingPeriod(d.spending?.last_5h)],['本周期 7days 金额',spendingPeriod(d.spending?.last_7d)],['周额度金额估算',weeklyEstimate(d.spending?.weekly_estimate)],['用量不完整',n(s.incomplete_usage)]])}<div class="actions"><button data-action="account-models-sync" data-id="${id}">获取模型</button><button data-action="quota-refresh" data-id="${id}">采集额度</button><button data-action="reset-open" data-id="${id}">查询与重置额度</button><button data-action="credential-refresh" data-id="${id}">刷新授权</button><button data-action="oauth-start" data-id="${id}">重新授权</button></div><section class="detail-section"><h3>官方额度池</h3>${d.quotas.length?table(['额度池','窗口','已用','到期时间','采集时间'],d.quotas.map(w=>`<tr><td>${esc(w.pool)}</td><td>${w.window_minutes||'—'} 分钟</td><td>${w.used_percent}%</td><td>${dt(w.resets_at)}</td><td>${dt(w.observed_at)}</td></tr>`)):empty('额度未知','点击采集额度以更新。')}</section><section class="detail-section"><div class="model-selection-head"><h3>模型范围 · ${accountModels(a).length}</h3><button class="small" data-action="account-models-open" data-id="${id}">编辑</button></div><div class="account-models detail-models">${modelChips(accountModels(a))||'<span class="muted">未选择模型</span>'}</div>${a.model_catalog?.error?`<div class="notice error">${esc(a.model_catalog.error.message)}</div>`:''}</section><section class="detail-section"><h3>客户端配置</h3>${fact([['Codex 版本',esc(a.profile.codex_version)],['安装 ID',esc(a.profile.installation_id)],['TLS',esc(a.profile.tls_backend)],['User-Agent',esc(a.profile.user_agent)]])}</section>`,{drawer:true});}
const resetBusy=new Set();
let resetPanel=null;
function resetStorageKey(id){return 'xxgate-reset-operation:'+id;}
function savedReset(id){try{return JSON.parse(localStorage.getItem(resetStorageKey(id))||'null');}catch{return null;}}
function forgetReset(id){localStorage.removeItem(resetStorageKey(id));}
function resetResultText(result){return ({reset:`额度已重置，恢复 ${n(result.windows_reset)} 个窗口。`,already_redeemed:'该重置已使用，未重复消耗次数。',nothing_to_reset:'当前额度无需重置，未消耗次数。',no_credit:'上游没有可用的重置次数。'})[result.code]||'重置结果待核实。';}
function resetPaint(panel){
 if(resetPanel!==panel||!dialog.open||!$('#reset-content'))return;
 const d=panel.data,s=d?.snapshot,next=d?.next_credit,pending=d?.pending,active=pending||panel.retry;
 const busy=resetBusy.has(panel.id),ready=!!active||(!panel.loading&&!panel.error&&!!next);
 const status=c=>c.expires_at&&new Date(c.expires_at)<=new Date()&&c.status==='available'?'已过期':({available:'可用',redeeming:'使用中',redeemed:'已使用',expired:'已过期'})[c.status]||c.status;
 $('#reset-content').innerHTML=`<div class="reset-overview"><div><span class="muted">剩余可用重置</span><strong>${s?n(d.usable_count):'—'}<small> 次</small></strong></div><button data-action="reset-refresh" data-id="${panel.id}" ${busy||panel.loading?'disabled':''}>${panel.loading?'查询中…':'重新查询'}</button></div>${s?`<p class="muted">查询时间：${dt(s.observed_at)} · 上游报告 ${n(s.available_count)} 次</p>`:''}${panel.notice?`<div class="notice" role="status">${esc(panel.notice)}</div>`:''}${panel.error?`<div class="notice error" role="alert">${esc(panel.error)}${s?'；以下保留最近成功查询结果。':''}</div>`:''}${active?'<div class="notice">上次操作的结果待核实，继续操作将使用原来的那次重置。</div>':''}
 <section class="reset-action"><div><h3>${active?'核实上次重置':'下一次重置'}</h3><p>${active?`凭据 ${esc(active.credit_id)}`:next?`${esc(next.title||next.id)}<br>${next.expires_at?'到期：'+dt(next.expires_at):'无到期时间'}`:'暂无可用重置'}</p><small>优先使用最早到期的次数，每次操作使用 1 次。</small></div><button class="primary" data-action="reset-consume" data-id="${panel.id}" ${!ready||busy?'disabled':''}>${busy?'处理中…':active?'核实并重试':'重置一次'}</button></section>
 <section class="detail-section"><h3>全部重置 · ${s?n(s.credits.length):'—'}</h3>${s?.credits.length?table(['重置','状态','获得时间','到期时间'],s.credits.map(c=>`<tr class="${c.id===next?.id?'reset-next':''}"><td><strong>${esc(c.title||'额度重置')}</strong>${c.id===next?.id?'<span class="tag good">优先使用</span>':''}<div class="sub mono">${esc(c.id)}</div><div class="sub">${c.reset_type==='codex_rate_limits'?'Codex 额度':esc(c.reset_type)}</div>${c.description?`<div class="sub">${esc(c.description)}</div>`:''}</td><td>${esc(status(c))}</td><td>${dt(c.granted_at)}</td><td>${c.expires_at?dt(c.expires_at):'无到期时间'}</td></tr>`)):empty(s?'没有重置记录':'尚未查询',panel.loading?'正在读取账户的重置次数…':'点击重新查询以获取最新记录。')}</section>`;
}
async function resetOpen(id,refresh=true,notice=''){
 const account=state.accounts.find(x=>x.account.id===id)?.account;
 const panel={id,data:null,loading:true,error:'',notice,retry:savedReset(id)};resetPanel=panel;
 modal('额度重置', '<div id="reset-content"><p class="notice">正在查询…</p></div>',{subtitle:account?.name||'',drawer:true});
 try{
  panel.data=await api(`/accounts/${id}/reset-credits${panel.retry?'?operation_id='+encodeURIComponent(panel.retry.id):''}`);
  if(panel.data.operation?.result){panel.notice=resetResultText(panel.data.operation.result);forgetReset(id);panel.retry=null;}
  else if(panel.retry&&!panel.data.operation&&!panel.data.pending){forgetReset(id);panel.retry=null;}
  resetPaint(panel);
  if(refresh){const fresh=await api(`/accounts/${id}/reset-credits/refresh`,'POST');panel.data=fresh;}
 }catch(e){panel.error=e.message;}
 finally{panel.loading=false;resetPaint(panel);}
 if(refresh)await render();
}
async function resetConsume(id){
 const panel=resetPanel;if(!panel||panel.id!==id||resetBusy.has(id))return;
 const operation=panel.data?.pending||panel.retry||{id:crypto.randomUUID(),credit_id:panel.data?.next_credit?.id};
 if(!operation.credit_id)return;
 localStorage.setItem(resetStorageKey(id),JSON.stringify({id:operation.id,credit_id:operation.credit_id}));
 panel.retry=operation;resetBusy.add(id);resetPaint(panel);
 let notice='';
 try{
  const result=await api(`/accounts/${id}/reset-credits/consume`,'POST',{operation_id:operation.id,expected_credit_id:operation.credit_id});
  notice=resetResultText(result.operation.result);
  if(result.refresh_errors?.length)notice+=' 最新额度或次数查询失败，可稍后重新查询。';
  forgetReset(id);
 }catch(e){notice=e.message;}
 finally{resetBusy.delete(id);}
 if(resetPanel?.id===id&&dialog.open&&$('#reset-content'))await resetOpen(id,false,notice);
 await render();
}

async function accountEdit(id){await loadGroups();const a=state.accounts.find(x=>x.account.id===id).account;state.accountEditor=a;modal('账户设置',`<form data-form="account-edit" data-id="${id}"><div class="form-grid">${field('账户名称','name',a.name,'text','','required maxlength="128"')}${field('并发上限','max_inflight',a.max_inflight,'number','','required min="1" max="1000"')}${groupSelection(a.group_ids)}<div class="field full source-policy"><label class="check"><input type="checkbox" name="codex_only" ${a.codex_only?'checked':''}><strong>仅允许 Codex</strong></label><small>开启后此账户只接收识别为 Codex 的请求。默认允许所有来源，包括未知客户端。</small><small>按入站 User-Agent 或 Codex 元数据识别；代理移除这些特征时会显示为未知。</small></div><label class="field full">User-Agent<input name="user_agent" value="${esc(a.profile.user_agent)}" required></label><label class="field">TLS 后端<select name="tls_backend"><option value="native" ${a.profile.tls_backend==='native'?'selected':''}>Native TLS</option><option value="rustls" ${a.profile.tls_backend==='rustls'?'selected':''}>Rustls</option></select></label></div>${formActions()}</form>`);}
async function accountImport(){await loadGroups();modal('导入已有授权',`<form data-form="account-import"><div class="form-grid">${field('账户名称','name','','text','','required maxlength="128"')}${groupSelection()}${field('上游账户 ID','upstream_account_id','','text','留空从凭证中读取')}<label class="field full">OAuth 凭证 JSON<textarea name="credentials" rows="7" required placeholder='{"access_token":"…","refresh_token":"…","id_token":"…","expires_at":null}' autocomplete="off" spellcheck="false"></textarea></label>${field('上游地址','upstream_base_url','https://chatgpt.com/backend-api/codex','url','','required')}</div><div class="notice">凭证加密保存。导入后账户处于停用状态，确认配置后手动启用。</div>${formActions('导入账户')}</form>`);}
async function oauthStart(id){await loadGroups();const a=id?state.accounts.find(x=>x.account.id===id)?.account:null;modal(id?'重新授权':'添加 ChatGPT 账户',`<form data-form="oauth" data-id="${id||''}"><div class="form-grid">${field('账户名称','name',a?.name||'','text','','required maxlength="128"')}<label class="field">授权方式<select name="method"><option value="browser">链接授权（推荐）</option><option value="device">设备码授权</option></select></label>${id?'':groupSelection()}</div><div class="notice">链接授权：复制授权链接到浏览器完成登录，再将跳转后的完整链接粘贴回来。</div>${formActions('继续')}</form>`);}
function watchOAuth(id,method){let polling=false;oauthTimer=setInterval(async()=>{if(polling)return;polling=true;try{const result=await api('/oauth/'+method+'/'+id);if(result.status==='completed'){clearInterval(oauthTimer);pendingBrowserFlow=null;modal('授权已完成',`<div class="notice">账户「${esc(result.account.name)}」授权已更新，当前${result.account.enabled?'已启用':'处于停用状态，可在账户列表中启用'}。</div><div class="form-actions"><button class="primary" data-action="close">完成</button></div>`);render();}else if(result.status==='failed'){clearInterval(oauthTimer);pendingBrowserFlow=null;$('#oauth-status').textContent=result.error.message;$('#oauth-status').classList.add('error');}}catch(e){clearInterval(oauthTimer);toast(e.message);}finally{polling=false;}},1500);}
function modelEdit(id){const m=state.models.find(x=>x.id===id);modal(m?'编辑模型':'添加模型',`<form data-form="model" data-id="${esc(id||'')}"><div class="form-grid">${field('对外模型名称','id',m?.id||'','text','接入方请求中的 model',`required ${m?'readonly':''}`)}${field('上游模型名称','upstream_model',m?.upstream.model||'','text','应与账户实际可调用的模型一致','required')}<div class="field full"><span>模型能力</span><div class="checks">${[['image_input','图片输入'],['image_generation','图片生成'],['tools','工具'],['reasoning','推理'],['fast','Fast']].map(([k,l])=>`<label class="check"><input type="checkbox" name="${k}" ${m?m.capabilities[k]?'checked':'':'checked'}>${l}</label>`).join('')}</div></div><label class="check"><input type="checkbox" name="enabled" ${!m||m.enabled?'checked':''}>启用模型</label></div>${formActions()}</form>`);}
function priceEdit(id){
 const m=state.models.find(x=>x.id===id)||state.models.find(x=>state.discovered?.some(d=>sameModel(d.model,x.upstream))&&!state.prices.some(p=>sameModel(p.model,x.upstream)))||state.models.find(x=>!state.prices.some(p=>sameModel(p.model,x.upstream)))||state.models[0];if(!m)return toast('请先从上游获取模型');
 const p=state.prices.find(x=>sameModel(x.model,m.upstream));
 const preferred=state.models.filter(m=>state.discovered?.some(x=>sameModel(x.model,m.upstream)));
 const manual=state.models.filter(m=>!preferred.includes(m));
 const options=items=>items.map(x=>`<option value="${esc(x.id)}" ${x.id===m.id?'selected':''}>${esc(x.id)}${state.prices.some(p=>sameModel(p.model,x.upstream))?' · 已有价格':' · 待填价'}</option>`).join('');
 const rates=`<div class="form-grid">${[['input_per_million','输入 Token'],['cached_input_per_million','缓存输入 Token'],['output_per_million','输出 Token'],['image_input_per_million','图片工具输入 Token'],['image_output_per_million','图片工具输出 Token'],['per_image','按张计费']].map(([k,l],i)=>field(l,k,p?.standard?.[k]??'','number',k==='per_image'?'人民币 / 张；与图片 Token 计价二选一':'人民币 / 百万 Token',`min="0" max="1000000" step="any" ${i<3?'required':''}`)).join('')}</div>`;
 modal('填写模型价格',`<form data-form="price" data-id="${esc(m.id)}"><label class="field">模型<select id="price-model-choice">${preferred.length?`<optgroup label="账户上游模型">${options(preferred)}</optgroup>`:''}${manual.length?`<optgroup label="已配置模型">${options(manual)}</optgroup>`:''}</select><small>上游模型：${esc(m.upstream.model)}</small></label><section class="form-section"><h3>标准价格</h3>${rates}</section><section class="form-section"><h3>Fast / Priority</h3>${field('Fast 倍率','fast_multiplier',p?.fast_multiplier??'2','number','Fast 费用 = 标准计价 × 倍率，适用于输入、缓存、输出及图片计价项。','required min="0.0001" max="100" step="any"')}</section>${formActions('保存价格')}</form>`,{subtitle:'模型名称直接选择，仅需填写价格和倍率'});
}
document.addEventListener('click',async event=>{
 if(event.target.closest('[data-billing]'))return;
 const el=event.target.closest('button,[data-action]');if(!el)return;
 if(el.dataset.page){navigate(el.dataset.page);return;}
 const a=el.dataset.action,id=el.dataset.id;if(!a)return;
 try{
  if(a==='close')return close();
  if(a==='refresh'&&state.page==='errors')ErrorCenter.refresh();
  if(a==='refresh')return render();
  if(a==='logout'){await api('/logout','POST');return loginPage();}
  if(a==='account-delete'||a==='key-delete'){
   const account=a==='account-delete',item=account?state.accounts.find(x=>x.account.id===id)?.account:state.keys.find(x=>x.id===id);
   return modal(account?'删除账户':'删除 API Key',`<p>确认删除「${esc(item?.name||id)}」？${account?'账户将停止接收新请求。':'此 Key 将立即失效。'}历史请求和审计记录会保留。</p><div class="form-actions"><button data-action="close">取消</button><button class="danger" data-action="delete-confirm" data-kind="${account?'accounts':'keys'}" data-id="${id}">确认删除</button></div>`);
  }
  if(a==='delete-confirm'){el.disabled=true;try{await api('/'+el.dataset.kind+'/'+id,'DELETE');close();toast('已删除');await render();}catch(error){el.disabled=false;throw error;}return;}
  if(a==='account-import')return await accountImport();
  if(a==='account-edit')return await accountEdit(id);
  if(a==='account-models-open')return await accountModelEdit(id);
  if(a==='account-models-all'||a==='account-models-none'){dialog.querySelectorAll('input[name="account_model"]').forEach(b=>{b.checked=a==='account-models-all';});updateModelSelection();return;}
  if(a==='account-models-refresh'){el.disabled=true;await api('/accounts/'+id+'/models/sync','POST');await accountModelEdit(id);await render();return;}
  if(a==='account-detail')return await accountDetail(id);
  if(a==='reset-open'||a==='reset-refresh')return await resetOpen(id);
  if(a==='reset-consume')return await resetConsume(id);
  if(a==='group-create'||a==='group-edit')return groupEdit(id);
  if(a==='group-delete'){el.disabled=true;await api('/groups/'+id,'DELETE');toast('分组已删除');return render();}
  if(a==='group-accounts'){state.accountGroupFilter=id;return navigate('accounts');}
  if(a==='oauth-start')return await oauthStart(id);
  if(a==='model-edit')return modelEdit(id);
  if(a==='price-edit')return priceEdit(id);
  if(a==='price-new')return priceEdit();
  if(a==='search-price-edit'){const p=await api('/search-price');state.searchPriceEditor=p;modal('Search 全局单价',`<form data-form="search-price">${field('每次成功请求（人民币）','per_call',p.per_call??'','number','留空表示未配置价格；0 表示免费。','min="0" max="1000000" step="0.00000001"')}<p class="muted">适用于所有账户和模型。一条请求包含多个查询命令也只计一次，历史费用不重算。</p>${formActions('保存单价')}</form>`);return;}
  if(a==='models-sync'){el.disabled=true;await api('/models/sync','POST');state.modelSyncRunning=true;await render();return;}
  if(a==='account-models-sync'){el.disabled=true;await api('/accounts/'+id+'/models/sync','POST');toast('账户模型已更新');await accountDetail(id);return;}
  if(a?.startsWith('errors-'))return await ErrorCenter.action(a,el);
  if(a==='request-detail'){event.preventDefault();return await requestDetail(id);}
  if(a==='cache-drops'){state.cacheDropsOnly=!state.cacheDropsOnly;return render();}
  if(a==='request-session'){close();state.filter={session_id:el.dataset.session,key_id:el.dataset.key};state.offset=0;state.cacheDropsOnly=false;if(state.page==='requests'){await render();window.scrollTo(0,0);return;}return navigate('requests');}
  if(a==='key-create'||a==='key-edit')return await keyEdit(id);
  if(a==='oauth-link-copy'){await navigator.clipboard.writeText($('#oauth-link').value);return toast('授权链接已复制');}
  if(a==='key-copy'){await navigator.clipboard.writeText($('#new-key').textContent);return toast('已复制密钥');}
  if(a==='usage-reset'){state.usageFilter={};return render();}
  if(a==='filter-reset'){state.filter={};state.cacheDropsOnly=false;state.offset=0;return render();}
  if(a==='request-prev'||a==='request-next'){state.offset=Math.max(0,state.offset+(a==='request-next'?25:-25));return render();}
  if(a==='audit-prev'||a==='audit-next'){state.auditOffset=Math.max(0,state.auditOffset+(a==='audit-next'?30:-30));return render();}
  el.disabled=true;
  if(a==='account-enabled'){await api(`/accounts/${id}/enabled`,'PUT',{enabled:el.dataset.enabled==='true'});await render();if(dialog.open)await accountDetail(id);}
  if(a==='key-enabled'){await api(`/keys/${id}/enabled`,'PUT',{enabled:el.dataset.enabled==='true'});toast('Key 状态已更新');await render();}
  if(a==='quota-refresh'){await api(`/accounts/${id}/quota`,'POST');toast('额度已更新');await accountDetail(id);}
  if(a==='credential-refresh'){await api(`/accounts/${id}/refresh`,'POST');toast('授权已更新');await accountDetail(id);}
  if(a==='cleanup'){await api('/cleanup','POST');toast('清理已调度');}
 }catch(e){toast(e.message);}finally{el.disabled=false;}
});
document.addEventListener('change',event=>{if(event.target.id==='account-group-filter'){state.accountGroupFilter=event.target.value;render();return;}if(event.target.id==='key-group-filter'){state.keyGroupFilter=event.target.value;render();return;}if(event.target.name==='account_model'){updateModelSelection();return;}if(event.target.id==='rewrite-unchanged'){filterRewrites();return;}if(event.target.id==='price-model-choice'){priceEdit(event.target.value);return;}if(event.target.id==='dashboard-account'){state.accountFilter=event.target.value;render();}});
document.addEventListener('submit',async event=>{
 const form=event.target;if(!form.dataset.form)return;event.preventDefault();const kind=form.dataset.form,id=form.dataset.id,fd=new FormData(form),v=Object.fromEntries(fd);const button=form.querySelector('button[type=submit]'),error=form.querySelector('.text-error');if(error)error.textContent='';if(button)button.disabled=true;
 try{
  if(kind==='login'){await api('/login','POST',v);form.reset();return shell();}
  if(kind==='usage-filter'){state.usageFilter={};for(const [k,x] of Object.entries(v)){if(x)state.usageFilter[k]=['from','to'].includes(k)?new Date(x).toISOString():x;}return render();}
  if(kind==='error-filter')return await ErrorCenter.submit(v);
  if(kind==='filter'){state.cacheDropsOnly=false;state.filter={};['kind','state','model','session_id','client_source'].forEach(k=>{if(v[k])state.filter[k]=v[k]});if(v.lookup)state.filter[v.lookup_kind]=v.lookup;state.offset=0;return render();}
  if(kind==='account-import'){const credentials=JSON.parse(v.credentials);await api('/accounts','POST',{name:v.name,credentials,upstream_account_id:v.upstream_account_id||null,upstream_base_url:v.upstream_base_url,group_ids:fd.getAll('group_id')});}
  if(kind==='account-edit'){const a=state.accountEditor;await api('/accounts/'+id,'PUT',{name:v.name,codex_only:fd.has('codex_only'),user_agent:v.user_agent,tls_backend:v.tls_backend,group_ids:fd.getAll('group_id'),version:a.version,max_inflight:Number(v.max_inflight),models:a.models,models_restricted:a.models_restricted||false});}
  if(kind==='account-models'){const a=state.modelEditor;await api('/accounts/'+id,'PUT',{version:a.version,name:a.name,max_inflight:a.max_inflight,models:fd.getAll('account_model'),models_restricted:true});}
  if(kind==='group')await api('/groups'+(id?'/'+id:''),id?'PUT':'POST',{name:v.name});
  if(kind==='key-edit')await api('/keys/'+id,'PUT',{name:v.name,group_id:v.group_id});
  if(kind==='key'){const d=await api('/keys','POST',v);modal('Key 已创建',`<p class="muted">请立即保存。关闭后无法再次查看完整密钥。</p><pre id="new-key" class="secret-box">${esc(d.secret)}</pre><div class="form-actions"><button data-action="key-copy">复制密钥</button><button class="primary" data-action="close">已保存</button></div>`);render();return;}
  if(kind==='model'){const m=state.models.find(x=>x.id===id);await api('/models','PUT',{id:v.id,upstream:{provider:'openai',access_kind:'codex_oauth',model:v.upstream_model},enabled:fd.has('enabled'),version:m?.version||0,capabilities:Object.fromEntries(['image_input','image_generation','tools','reasoning','fast'].map(k=>[k,fd.has(k)]))});}
  if(kind==='price'){const m=state.models.find(x=>x.id===id);const rates=Object.fromEntries(['input_per_million','cached_input_per_million','output_per_million','image_input_per_million','image_output_per_million','per_image'].map(k=>[k,v[k]||null]));await api('/prices','PUT',{version:0,model:m.upstream,standard:rates,fast_multiplier:v.fast_multiplier});}
  if(kind==='search-price')await api('/search-price','PUT',{version:state.searchPriceEditor.version,per_call:v.per_call===''?null:v.per_call});
  if(kind==='settings'){const c={...state.config,...Object.fromEntries(Object.entries(v).map(([k,x])=>[k,Number(x)]))};await api('/settings','PUT',c);toast('配置已保存并立即生效');await render();return;}
  if(kind==='oauth'){
   const method=v.method||'browser';
   const d=await api('/oauth/'+method,'POST',{name:v.name,account_id:id||null,...(id?{}:{group_ids:fd.getAll('group_id')})});
   if(method==='browser'){
    pendingBrowserFlow=d.id;
    modal('链接授权',`<form data-form="oauth-callback" data-id="${d.id}"><label class="field">1. 复制授权链接，在浏览器中打开<textarea id="oauth-link" readonly rows="3" spellcheck="false">${esc(d.authorization_url)}</textarea></label><div class="actions"><button type="button" data-action="oauth-link-copy">复制授权链接</button><a href="${esc(d.authorization_url)}" target="_blank" rel="noopener noreferrer">打开授权页面 ↗</a></div><div class="notice">2. 登录并授权后，复制地址栏中的完整回调链接。即使 localhost 页面无法打开，也可以复制该链接。</div><label class="field">3. 粘贴回调链接<textarea name="callback_url" rows="4" autocomplete="off" spellcheck="false" required placeholder="http://localhost:1455/auth/callback?code=…&state=…"></textarea><small>使用当前授权链接生成的回调地址，15 分钟内有效。</small></label>${formActions('完成授权')}</form>`,{subtitle:'授权链接 → 浏览器登录 → 粘贴回调链接'});
   }else{
    modal('完成设备授权',`<div class="device"><p>打开授权页面，登录对应账户并输入以下代码。</p><div class="device-code">${esc(d.user_code)}</div><a href="${esc(d.verification_uri)}" target="_blank" rel="noopener noreferrer">打开 OpenAI 授权页面 ↗</a><p class="notice" id="oauth-status">等待授权完成…</p></div>`);
    watchOAuth(d.id,'device');
   }
   return;
  }
  if(kind==='oauth-callback'){
   await api('/oauth/browser/'+id+'/complete','POST',{callback_url:v.callback_url});
   pendingBrowserFlow=null;form.reset();
   modal('正在完成授权','<p class="notice" id="oauth-status">正在兑换凭证并保存账户，请稍候…</p><div class="form-actions"><button data-action="close">关闭</button></div>');
   watchOAuth(id,'browser');return;
  }
  close();toast('已保存');await render();
 }catch(e){if(error)error.textContent=e.message;else toast(e.message);}finally{if(button)button.disabled=false;}
});
window.addEventListener('hashchange',()=>{const page=location.hash.slice(1);if(pages.some(p=>p[0]===page)&&page!==state.page)navigate(page);});
setInterval(()=>{if(state.auth&&!document.hidden&&!dialog.open&&(['dashboard','accounts','requests'].includes(state.page)||(state.page==='models'&&state.modelSyncRunning))&&!document.querySelector('main input:focus,main select:focus'))render(true);},10000);
(async()=>{try{await api('/session');const page=location.hash.slice(1);if(pages.some(p=>p[0]===page))state.page=page;shell();}catch{loginPage();}})();
