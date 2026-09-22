(function(root){
 'use strict';
 const plans={free:'Free',plus:'Plus',pro:'Pro',go:'Go',team:'Team',business:'Business',enterprise:'Enterprise',edu:'Edu'};
 function planLabel(plan){return Object.hasOwn(plans,plan)?plans[plan]:'套餐未知';}
 function windowLabel(minutes){return minutes===300?'5h':minutes>0&&minutes%1440===0?`${minutes/1440}days`:minutes>0?`${minutes} 分钟`:'未知';}
 function periods(plan,quotas,spending){
  const windows=(quotas||[]).map(q=>q.window||q).filter(w=>w.pool==='codex'&&[300,10080,43200].includes(w.window_minutes));
  const observed=minutes=>Math.max(-Infinity,...windows.filter(w=>minutes.includes(w.window_minutes)).map(w=>Date.parse(w.observed_at)||0));
  const monthly=observed([43200]),weekly=observed([300,10080]);
  // Actual observed windows take precedence over a potentially older plan claim.
  const minutes=monthly!==-Infinity&&monthly>=weekly?[43200]:weekly!==-Infinity?[300,10080]:plan==='free'?[43200]:Object.hasOwn(plans,plan)?[300,10080]:[];
  return minutes.map(m=>({minutes:m,label:windowLabel(m),period:spending?.[{300:'last_5h',10080:'last_7d',43200:'last_30d'}[m]]}));
 }
 function estimate(plan,quotas,spending){
  const m=periods(plan,quotas,spending).at(-1)?.minutes;
  return m?{minutes:m,label:windowLabel(m),value:m===43200?spending?.monthly_estimate:spending?.weekly_estimate}:null;
 }
 const api={planLabel,windowLabel,periods,estimate};
 if(typeof module==='object'&&module.exports)module.exports=api;else root.AccountQuotas=api;
})(typeof globalThis==='undefined'?this:globalThis);
