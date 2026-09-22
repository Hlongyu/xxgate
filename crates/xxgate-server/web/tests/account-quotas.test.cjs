const {test}=require('node:test');
const assert=require('node:assert/strict');
const q=require('../account-quotas.js');
const window=(minutes,time='2026-09-22T00:00:00Z',pool='codex')=>({window:{pool,window_minutes:minutes,observed_at:time}});
test('Free uses the 30-day cycle, while Plus and Pro retain 5h and 7days',()=>{
 const spending={last_30d:{cny:'30'},last_5h:{cny:'5'},last_7d:{cny:'7'},monthly_estimate:{total_cny:'50'}};
 assert.deepEqual(q.periods('free',[window(43200)],spending),[{minutes:43200,label:'30days',period:spending.last_30d}]);
 assert.equal(q.estimate('free',[window(43200)],spending).value,spending.monthly_estimate);
 for(const plan of ['plus','pro'])assert.deepEqual(q.periods(plan,[window(300),window(10080)],spending).map(p=>p.minutes),[300,10080]);
});
test('actual windows override stale plan claims and previous subscription windows',()=>{
 assert.deepEqual(q.periods('plus',[window(43200)],{}).map(p=>p.minutes),[43200]);
 assert.deepEqual(q.periods('free',[window(43200,'2026-09-20T00:00:00Z'),window(300),window(10080)],{}).map(p=>p.minutes),[300,10080]);
 assert.deepEqual(q.periods(null,[window(10080,'2026-09-20T00:00:00Z'),window(43200)],{}).map(p=>p.minutes),[43200]);
});
test('missing metadata and separate pools do not fabricate a quota or a Free plan',()=>{
 for(const plan of [null,undefined,'<img>','constructor'])assert.equal(q.planLabel(plan),'套餐未知');
 assert.equal(q.planLabel('free'),'Free');assert.equal(q.planLabel('plus'),'Plus');assert.equal(q.planLabel('pro'),'Pro');
 assert.deepEqual(q.periods(null,[window(0),window(43200,undefined,'spark')],{}),[]);
 assert.equal(q.periods('free',[],{}).length,1);assert.equal(q.periods('free',[],{})[0].period,undefined);
 assert.equal(q.estimate(null,[],{}),null);assert.equal(q.windowLabel(43200),'30days');assert.equal(q.windowLabel(0),'未知');
});
