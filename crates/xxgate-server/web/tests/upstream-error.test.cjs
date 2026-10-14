const test=require('node:test');
const assert=require('node:assert/strict');
const fs=require('node:fs');
const vm=require('node:vm');
const path=require('node:path');

test('original upstream errors are escaped, truncation is visible, legacy details stay readable',()=>{
 const source=fs.readFileSync(path.join(__dirname,'../app.js'),'utf8');
 const context={};vm.createContext(context);
 const escape=source.split('\n').find(line=>line.startsWith('function esc('));
 const render=source.slice(source.indexOf('function originalUpstreamError('),source.indexOf('function memoryDiagnostics('));
 vm.runInContext(escape+'\n'+render,context);
 assert.equal(context.originalUpstreamError({}), '');
 assert.equal(context.originalUpstreamError({upstream_error:{code:'server_error'}}), '');
 const html=context.originalUpstreamError({upstream_error:{original:{source:'sse',truncated:true,fields:{message:'<script>alert(1)</script>',code:'unknown_code'}}}});
 assert.ok(!html.includes('<script>'));
 assert.match(html,/&lt;script&gt;/);
 assert.match(html,/unknown_code/);
 assert.match(html,/已截断/);
});
