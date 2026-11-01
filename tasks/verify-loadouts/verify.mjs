import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, access, unlink, utimes } from 'node:fs/promises';
import { spawn, spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const scratch = await mkdtemp(join(tmpdir(), 'hoard-loadouts-'));
const source = join(scratch, 'pose-project');
const env = { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
    RAYON_NUM_THREADS: '2', TOKIO_WORKER_THREADS: '2', GIT_LFS_SKIP_SMUDGE: '1',
    HOARD_NODE_TOKEN: 'synthetic-owner-token', HF_HOME:join(scratch,'hf-client') };
delete env.HF_TOKEN;
const pause = ms => new Promise(resolve => setTimeout(resolve, ms));
function git(args, cwd = source) {
    const result = spawnSync('git', args, { cwd, env, encoding: 'utf8' });
    assert.equal(result.status, 0, result.stderr);
    return result.stdout.trim();
}
await mkdir(source);
git(['init', '--quiet', '--initial-branch=main']);
git(['config', 'user.name', 'Hoard Example']);git(['config','user.email','example@invalid.test']);
git(['config','commit.gpgsign','false']);
await writeFile(join(source,'README.md'),'# Pose project\n\nA searchable animation toolkit.\n');
git(['add','.']);git(['commit','--quiet','-m','Example project']);
const empty = join(scratch,'empty');await mkdir(empty);
const missingPath = join(scratch,'missing-folder');
const reserve = net.createServer();
await new Promise(resolve => reserve.listen(0,'127.0.0.1',resolve));
const port = reserve.address().port;
await new Promise(resolve => reserve.close(resolve));
assert.notEqual(port,8080);
const origin = `http://127.0.0.1:${port}`;
const config = join(scratch,'hoard.toml');
const dataDir = join(scratch,'data','managed-model-links-'+'long-path-'.repeat(10));
await writeFile(config, `[server]\nbind="127.0.0.1:${port}"\npublic_url="${origin}"\ndata_dir="${dataDir}"\n[node]\nname="first-run"\ndiscovery="none"\n[index]\nhash_policy="now"\nhash_threads=2\nwatch=true\nreconcile_interval_secs=0\n[mirror]\noffline=true\n`);
let server, browser, socket;
let serverLog = '';
const closeBrowser = () => {
    if (socket?.readyState === WebSocket.OPEN) {
        socket.onmessage = null;
        socket.send(JSON.stringify({ id: 0, method: 'Browser.close' }));
    }
    socket?.close();
};
const exceptions = [];
const startServer = async () => {
    server=spawn('nice',['-n','10',join(root,'target/debug/hoard'),'serve','--config',config],{cwd:scratch,env,stdio:['ignore','pipe','pipe']});
    server.stdout.on('data',d=>serverLog+=d);server.stderr.on('data',d=>serverLog+=d);
    for(let i=0;i<200;i++) {
        if(server.exitCode!==null)throw new Error(serverLog);
        try {if((await fetch(origin+'/healthz')).ok)return;}catch{}
        await pause(100);
    }
    throw new Error('Server did not become ready: '+serverLog);
};
const stop = async child => {
    if(!child || child.exitCode!==null || child.signalCode)return;
    await new Promise(resolve=>{const timer=setTimeout(()=>child.kill('SIGKILL'),5000);child.once('exit',()=>{clearTimeout(timer);resolve();});child.kill('SIGTERM');});
};
for (const signal of ['SIGINT', 'SIGTERM']) {
    process.once(signal, async () => {
        closeBrowser();
        await Promise.allSettled([stop(browser), stop(server)]);
        process.exit(130);
    });
}
try {
    await startServer();
    browser = spawn('nice',['-n','10',process.env.HOARD_BROWSER || '/usr/bin/brave-browser','--headless=new','--no-sandbox',
        '--disable-gpu','--disable-background-networking','--disable-component-update',
        '--no-first-run','--disable-sync','--disable-extensions',
        '--window-size=1600,1000',`--user-data-dir=${scratch}/browser`,'--remote-debugging-port=0','about:blank'],
        {stdio:['ignore','ignore','ignore']});
    let debugPort;
    for(let i=0;i<100;i++) {
        try {debugPort=(await readFile(join(scratch,'browser/DevToolsActivePort'),'utf8')).split('\n')[0];break;}catch{}
        await pause(100);
    }
    assert(debugPort,'browser debugging port missing');
    const pages = await (await fetch(`http://127.0.0.1:${debugPort}/json/list`)).json();
    socket = new WebSocket(pages.find(p=>p.type==='page').webSocketDebuggerUrl);
    await new Promise((resolve,reject)=>{socket.onopen=resolve;socket.onerror=reject;});
    let sequence = 0;
    const pending = new Map();
    socket.onmessage = event => {
        const message = JSON.parse(event.data);
        if(message.method==='Runtime.exceptionThrown') exceptions.push(message.params.exceptionDetails);
        if(message.id) {
            const task=pending.get(message.id); pending.delete(message.id);
            if(message.error) task.reject(new Error(JSON.stringify(message.error))); else task.resolve(message.result);
        }
    };
    const send = (method,params={})=>new Promise((resolve,reject)=>{
        const id=++sequence; pending.set(id,{resolve,reject}); socket.send(JSON.stringify({id,method,params}));
    });
    const evaluate = async expression => {
        const value=await send('Runtime.evaluate',{expression,awaitPromise:true,returnByValue:true});
        if(value.exceptionDetails) throw new Error(JSON.stringify(value.exceptionDetails));
        return value.result.value;
    };
    const wait = async expression => {
        for(let i=0;i<150;i++){if(await evaluate(expression))return;await pause(100);}
        throw new Error('Timed out: '+expression+'\n'+await evaluate('document.body.innerText'));
    };
    await send('Runtime.enable'); await send('Page.enable');
    const press=async(key,code=key)=>{await send('Input.dispatchKeyEvent',{type:'keyDown',key,code,...(key==='Enter'?{windowsVirtualKeyCode:13,text:'\r'}:key==='ArrowDown'?{windowsVirtualKeyCode:40}:key==='ArrowUp'?{windowsVirtualKeyCode:38}:{})});await send('Input.dispatchKeyEvent',{type:'keyUp',key,code,...(key==='Enter'?{windowsVirtualKeyCode:13}:{})});};
    const navigate=async hash=>evaluate('location.hash='+JSON.stringify(hash));
    let screenshots=0;
    const screenshot=async name=>{await evaluate('document.fonts.ready');await pause(250);const shot=await send('Page.captureScreenshot',{format:'png'});await writeFile(join(scratch,name+'.png'),Buffer.from(shot.data,'base64'));screenshots++;};
    await send('Page.navigate',{url:origin+'/'});
    await wait('location.hash==="#/sources"&&!!document.querySelector("#source-welcome")');
    const readApi=async path=>(await fetch(origin+'/api/v1'+path)).json();
    const writeApi=async(path,body,method='POST')=>{
        const response=await fetch(origin+'/api/v1'+path,{method,headers:{'content-type':'application/json',authorization:'Bearer synthetic-owner-token'},body:body===undefined?undefined:JSON.stringify(body)});
        const payload=await response.json();assert(response.ok,JSON.stringify(payload));return payload;
    };
    const fixtureRoot=join(scratch,'model-files');
    const names=[
        ['diffusion_models','qwen_image_2.1_int8_convrot.safetensors','ckpt'],
        ['text_encoders','qwen3vl_8b_int8_convrot.safetensors','llm'],
        ['text_encoders/nested','qwen3.5_9b_qwen_image_2.1_pe_t2i.int8_convrot.safetensors','llm'],
        ['vae','qwen_image_2.1_vae_bf16.safetensors','vae']
    ];
    const fixtures=new Map();
    for(const [index,[folder,name]] of names.entries()) {
        await mkdir(join(fixtureRoot,folder),{recursive:true});
        const header=Buffer.from(JSON.stringify({__metadata__:{ss_network_dim:'16',ss_base_model_version:'sdxl_base_v1-0'},'lora_unet_layer.lora_down.weight':{dtype:'F16',shape:[16,8],data_offsets:[0,256]},'lora_unet_layer.lora_up.weight':{dtype:'F16',shape:[8,16],data_offsets:[256,512]}}));
        const size=Buffer.alloc(8);size.writeBigUInt64LE(BigInt(header.length));
        const bytes=Buffer.concat([size,header,Buffer.alloc(512,index+1)]);
        fixtures.set(name,bytes);await writeFile(join(fixtureRoot,folder,name),bytes);
    }
    const unrelated=Buffer.from(fixtures.values().next().value);unrelated[unrelated.length-1]=99;
    await writeFile(join(fixtureRoot,'unrelated.safetensors'),unrelated);
    await writeApi('/sources',{name:'models',path:fixtureRoot,kind:'loose'});
    let artifacts;
    for(let i=0;i<150;i++){
        artifacts=(await readApi('/search?limit=10')).resources.filter(row=>row.artifact_id);
        if(artifacts.length===5)break;await pause(100);
    }
    assert.equal(artifacts.length,5);
    for(const [,name,role] of names){
        const model=artifacts.find(row=>row.name===name);assert(model,name);
        await writeApi('/triage/'+model.artifact_id+'/confirm',{role,base:'qwen'});
    }
    await navigate('/sources');await wait('!!document.querySelector(".owner-access")');
    await evaluate('document.querySelector("#source-sharing").open=true;document.querySelector("[name=owner_token]").value="synthetic-owner-token";document.querySelector(".owner-access").requestSubmit()');
    await navigate('/index?q=qwen');await wait('document.querySelectorAll(".trow").length===4');
    await screenshot('index-models-before');
    const ids=artifacts.filter(row=>fixtures.has(row.name)).map(row=>row.artifact_id);
    await evaluate('document.querySelectorAll(".trow").forEach(row=>{row.click();window.dispatchEvent(new KeyboardEvent("keydown",{key:" ",bubbles:true}))})');
    await evaluate('document.querySelector("#add-to-loadout").click()');
    await wait('document.querySelector(".loadout-pending-label")?.textContent.includes("4 selected")');
    await evaluate('document.querySelector("#loadout-new-name").value="Qwen Image 2.1 Text to Video";document.querySelector(".drawer-new button").click()');
    await wait('document.querySelectorAll(".loadout-member").length===4&&!document.querySelector("#loadout-copy").disabled');
    await writeFile(join(scratch,'collection-metrics.json'),JSON.stringify(await evaluate('(()=>{const body=document.querySelector(".drawer-body").getBoundingClientRect();const rows=[...document.querySelectorAll(".loadout-member")].map(row=>row.getBoundingClientRect());return {viewport:[innerWidth,innerHeight],rowHeights:rows.map(row=>row.height),fullyVisible:rows.filter(row=>row.top>=body.top&&row.bottom<=body.bottom).length,bodyHeight:body.height}})()'),null,2));
    const collection=(await readApi('/loadouts'))[0];
    const doc=await readApi('/loadouts/'+collection.id);
    assert.equal(doc.items.length,4);
    assert.equal(doc.items.filter(item=>item.role==='llm').length,2);
    for(const item of doc.items){
        const [folder,name]=names.find(([,name])=>name===item.path.split('/').at(-1));
        assert.equal(item.destination,folder+'/'+name);
    }
    assert(!(await evaluate('!!document.querySelector(".drawer-apply")')));
    await evaluate('document.querySelector("#loadout-rename").click();document.querySelector(".loadout-member").open=true;document.querySelector(".drawer-body").style.maxHeight="200px";window.collectionShell=document.querySelector(".drawer");window.collectionInput=document.querySelector("#loadout-name");window.pathInput=document.querySelector(".loadout-member input");window.pathDraft="diffusion_models/unsaved.safetensors";window.pathInput.value=window.pathDraft;window.pathInput.dispatchEvent(new Event("input"));window.pathInput.focus();window.pathInput.setSelectionRange(2,8);document.querySelector(".drawer-body").scrollTop=80;window.collectionScroll=document.querySelector(".drawer-body").scrollTop');
    assert(await evaluate('document.querySelector("#loadout-copy").disabled'));
    await evaluate('document.querySelector(".loadout-member").open=false');
    assert(await evaluate('document.querySelector(".loadout-member summary").textContent.includes("UNSAVED")'));
    await evaluate('document.querySelector(".loadout-member").open=true;window.pathInput.focus();window.pathInput.setSelectionRange(2,8);document.querySelector(".drawer-body").scrollTop=window.collectionScroll');
    await evaluate('document.querySelector("#loadout-name").value="Qwen Image 2.1 collection";document.querySelector("#loadout-name").dispatchEvent(new Event("input"));[...document.querySelectorAll(".loadout-name-form button")][0].click()');
    await wait('document.querySelector("#loadout-name").value==="Qwen Image 2.1 collection"&&!document.querySelector(".loadout-name-form button").disabled');
    assert(await evaluate('window.collectionShell===document.querySelector(".drawer")&&window.collectionInput===document.querySelector("#loadout-name")&&window.pathInput===document.querySelector(".loadout-member input")'));
    assert.equal(await evaluate('window.pathInput.value'),'diffusion_models/unsaved.safetensors');
    assert.equal(await evaluate('document.activeElement===window.pathInput'),true);
    assert.equal(await evaluate('window.pathInput.selectionStart'),2);
    assert.equal(await evaluate('document.querySelector(".drawer-body").scrollTop'),await evaluate("window.collectionScroll"));
    assert(await evaluate("window.collectionScroll>0"));
    await evaluate('window.pathInput.value="../escape";window.pathInput.dispatchEvent(new Event("input"));document.querySelector(".loadout-path-form button").click()');
    await wait('document.querySelector(".loadout-error").textContent.includes("portable relative")');
    assert.equal(await evaluate('window.pathInput.value'),'../escape');
    const original=doc.items[0].destination;
    await evaluate('window.pathInput.value='+JSON.stringify(original)+';window.pathInput.dispatchEvent(new Event("input"));document.querySelector(".loadout-path-form button").click()');
    await wait('!document.querySelector(".loadout-path-form button").disabled&&!document.querySelector(".loadout-error").textContent');
    await evaluate('document.querySelector(".drawer-body").style.maxHeight="";if(!document.querySelector(".loadout-rename").hidden)[...document.querySelectorAll(".loadout-name-form button")].find(button=>button.textContent==="CANCEL").click()');
    const clientRoot=join(scratch,"client models user's files");
    await evaluate('document.querySelector("#loadout-download-root").value='+JSON.stringify(clientRoot)+';document.querySelector("#loadout-download-root").dispatchEvent(new Event("input"));document.querySelector("#loadout-script-format").value="shell";document.querySelector("#loadout-script-format").dispatchEvent(new Event("change"))');
    await wait('!document.querySelector("#loadout-copy").disabled');
    const script=await readApi('/loadouts/'+collection.id+'/download?'+new URLSearchParams({format:'shell',root:clientRoot}));
    const scriptPath=join(scratch,'download.sh');await writeFile(scriptPath,script.text);
    const runScript=async(scriptFile=scriptPath)=>{
        const output=await new Promise((resolve,reject)=>{const child=spawn('sh',[scriptFile],{env:{...env,HF_HUB_DISABLE_TELEMETRY:'1',HF_DEBUG:'1'},stdio:['ignore','pipe','pipe']});let log='';child.stdout.on('data',d=>log+=d);child.stderr.on('data',d=>log+=d);child.on('error',reject);child.on('exit',code=>resolve({code,log}));});
        await writeFile(join(scratch,'download-attempt.log'),output.log);
        assert.equal(output.code,0,output.log);return output.log;
    };
    const downloadLog=await runScript();await writeFile(join(scratch,'download.log'),downloadLog);
    for(const item of doc.items)assert.deepEqual(await readFile(join(clientRoot,item.destination)),fixtures.get(item.path.split('/').at(-1)));
    await assert.rejects(access(join(clientRoot,'unrelated.safetensors')));
    await writeFile(join(clientRoot,doc.items[0].destination),'existing client model');
    const repeatLog=await runScript();assert(repeatLog.includes('Preserved existing file'));
    assert.equal(await readFile(join(clientRoot,doc.items[0].destination),'utf8'),'existing client model');
    for(const [folder,name] of names)assert.deepEqual(await readFile(join(fixtureRoot,folder,name)),fixtures.get(name));
    await assert.rejects(access(join(dataDir,'profiles')));
    await evaluate('document.querySelector(".loadout-member .loadout-remove").click()');
    await wait('document.querySelectorAll(".loadout-member").length===3');
    const removed=doc.items[0];
    await evaluate('document.querySelector("#loadout-close").click()');
    await navigate('/index?q='+encodeURIComponent(removed.name));
    await wait('document.querySelectorAll(".trow").length===1');
    await evaluate('document.querySelector("#add-to-loadout").click()');
    await wait('!!document.querySelector("#loadout-add-pending")&&!document.querySelector("#loadout-add-pending").disabled');
    await evaluate('document.querySelector("#loadout-add-pending").click()');
    await wait('document.querySelectorAll(".loadout-member").length===4&&!document.querySelector("#loadout-copy").disabled');
    for(const width of [1180,1600])for(const theme of ['light','dark']){
        await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:false});
        await evaluate('import("/js/theme.js").then(m=>m.updateSettings('+JSON.stringify({theme})+'))');
        await evaluate('document.querySelector("#loadout-script-format").value="powershell";document.querySelector("#loadout-script-format").dispatchEvent(new Event("change"));document.querySelector(".drawer-body").scrollTop=0');
        await wait('!document.querySelector("#loadout-copy").disabled');
        await evaluate('document.querySelectorAll(".loadout-member").forEach(row=>row.open=false)');
        await screenshot('collection-'+width+'-'+theme);
        assert(await evaluate('document.querySelector(".drawer").scrollWidth<=document.querySelector(".drawer").clientWidth'));
        assert(await evaluate('document.querySelector("#loadout-copy").getBoundingClientRect().bottom<=innerHeight'));
    }
    const savedScript=await readApi('/loadouts/'+collection.id+'/download?'+new URLSearchParams({format:'powershell',root:clientRoot}));
    const savedFolder=join(scratch,'browser-downloads');await mkdir(savedFolder);
    await send('Browser.setDownloadBehavior',{behavior:'allow',downloadPath:savedFolder});
    await evaluate('document.querySelector("#loadout-save-script").click()');
    let savedBytes;
    for(let i=0;i<100;i++){try{savedBytes=await readFile(join(savedFolder,savedScript.filename));break;}catch{}await pause(100);}
    assert(savedBytes,'downloaded PowerShell script missing');
    assert.deepEqual(savedBytes.subarray(0,3),Buffer.from([0xef,0xbb,0xbf]));
    assert.equal(savedBytes.subarray(3).toString('utf8'),savedScript.text);
    await evaluate('document.querySelectorAll(".loadout-member").forEach(row=>row.open=row.item.name.includes("qwen3.5_9b"));document.querySelector(".drawer-body").scrollTop=0');
    assert(await evaluate('document.querySelector(".loadout-member[open] .loadout-member-source").textContent.includes("nested/qwen3.5_9b")'));
    await screenshot('collection-model-expanded');
    await evaluate('document.querySelectorAll(".loadout-member").forEach(row=>row.open=false)');
    await evaluate('window.renameFetch=window.fetch;window.fetch=(input,options)=>String(input).startsWith("/api/v1/loadouts/")&&options?.method==="PUT"?new Promise(resolve=>window.finishRename=async()=>resolve(await window.renameFetch(input,options))):window.renameFetch(input,options);document.querySelector("#loadout-rename").click();document.querySelector("#loadout-name").value="Qwen Image 2.1 collection (saved)";document.querySelector("#loadout-name").dispatchEvent(new Event("input"));document.querySelector(".loadout-name-form button").click()');
    await wait('typeof window.finishRename==="function"');
    await evaluate('document.querySelector("#loadout-close").click();window.finishRename();window.fetch=window.renameFetch');
    assert(!(await evaluate('!!document.querySelector(".drawer")')));
    for(let i=0;i<100;i++){if((await readApi('/loadouts/'+collection.id)).name.endsWith('(saved)'))break;await pause(100);}
    assert.equal((await readApi('/loadouts/'+collection.id)).name,'Qwen Image 2.1 collection (saved)');
    await evaluate('document.querySelector("#loadout-btn").click()');
    await wait('document.querySelectorAll(".loadout-member").length===4&&!document.querySelector("#loadout-rename").disabled');
    assert.equal(exceptions.length,0,JSON.stringify(exceptions));
    const capacityNames=[];
    await mkdir(join(fixtureRoot,'capacity'));
    for(let i=0;i<20;i++){
        const name='capacity_model_'+String(i+1).padStart(2,'0')+'.safetensors';
        const bytes=Buffer.from(fixtures.values().next().value);bytes[bytes.length-1]=i+30;
        await writeFile(join(fixtureRoot,'capacity',name),bytes);capacityNames.push(name);
    }
    await writeApi('/scan',{source:'models',hash:'now'});
    let capacityModels=[];
    for(let i=0;i<150;i++){
        capacityModels=(await readApi('/search?q=capacity_model&limit=30')).resources.filter(row=>row.artifact_id);
        if(capacityModels.length===20)break;await pause(100);
    }
    assert.equal(capacityModels.length,20);
    const capacity=await writeApi('/loadouts',{name:'Condensed capacity collection'});
    await writeApi('/loadouts/'+capacity.id+'/members',{artifact_ids:capacityModels.map(row=>row.artifact_id)});
    await evaluate('document.querySelector("#loadout-close").click()');
    await navigate('/loadout/'+capacity.id);await wait('document.querySelectorAll(".loadout-member").length===20');
    assert(await evaluate('document.querySelector(".loadout-rename").hidden'));
    assert(await evaluate('document.querySelector(".loadout-create").hidden'));
    const capacityMetrics=[];
    for(const height of [1000,768,720])for(const theme of ['light','dark']){
        await send('Emulation.setDeviceMetricsOverride',{width:1180,height,deviceScaleFactor:1,mobile:false});
        await evaluate('import("/js/theme.js").then(m=>m.updateSettings('+JSON.stringify({theme,density:'Condensed'})+'))');
        const metrics=await evaluate('(()=>{const body=document.querySelector(".drawer-body");body.scrollTop=0;const bounds=body.getBoundingClientRect();const rows=[...document.querySelectorAll(".loadout-member")].map(row=>row.getBoundingClientRect());return {height:innerHeight,rowHeights:rows.map(row=>row.height),visible:rows.filter(row=>row.top>=bounds.top&&row.bottom<=bounds.bottom).length,copyVisible:document.querySelector("#loadout-copy").getBoundingClientRect().bottom<=innerHeight}})()');
        assert(metrics.rowHeights.every(value=>value<=34));
        assert(metrics.visible>=(height===1000?20:height===768?12:10),JSON.stringify(metrics));assert(metrics.copyVisible);
        capacityMetrics.push({...metrics,theme});await screenshot('collection-capacity-'+height+'-'+theme);
    }
    await writeFile(join(scratch,'capacity-metrics.json'),JSON.stringify(capacityMetrics,null,2));
    await evaluate('document.querySelector(".loadout-member summary").focus()');await press('Enter');
    assert(await evaluate('document.querySelector(".loadout-member").open'));
    assert(await evaluate('document.querySelector(".loadout-member-source").getBoundingClientRect().height>0'));
    assert(await evaluate('document.querySelector(".loadout-member summary").title.includes("Download:")'));
    assert(await evaluate('Math.abs(document.querySelector(".loadout-path-form input").getBoundingClientRect().height-document.querySelector(".loadout-path-form button").getBoundingClientRect().height)<1'));
    await screenshot('collection-capacity-expanded');
    await press('Enter');assert(!(await evaluate('document.querySelector(".loadout-member").open')));
    await evaluate('document.querySelector("#loadout-rename").click()');
    assert(await evaluate('document.activeElement.id==="loadout-name"'));
    assert(await evaluate('Math.abs(document.querySelector("#loadout-name").getBoundingClientRect().height-document.querySelector(".loadout-name-form button").getBoundingClientRect().height)<1'));
    await evaluate('[...document.querySelectorAll(".loadout-name-form button")].find(button=>button.textContent==="CANCEL").click()');
    assert(await evaluate('document.querySelector(".loadout-rename").hidden&&document.activeElement.id==="loadout-rename"'));
    await evaluate('document.querySelector("#loadout-delete").click();document.querySelector(".loadout-delete-confirm button").click()');
    await wait('document.querySelectorAll(".loadout-member").length===4');
    assert(!(await readApi("/loadouts")).some(row=>row.id===capacity.id));
    for(const name of capacityNames)await access(join(fixtureRoot,'capacity',name));
    await evaluate('document.querySelector("#loadout-close").click()');
    await send('Emulation.setDeviceMetricsOverride',{width:1600,height:1000,deviceScaleFactor:1,mobile:false});
    await navigate("/index?q=qwen");
    await wait('document.querySelectorAll(".trow").length===5');
    for(const width of [1180,1600])for(const theme of ['light','dark'])
    for(const density of ['Condensed','Roomy'])for(const showInspector of [true,false])
    for(const view of ['TABLE','CARDS']){
        await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:false});
        await evaluate('import("/js/theme.js").then(m=>m.updateSettings('+JSON.stringify({theme,density,showInspector})+'))');
        await evaluate('[...document.querySelectorAll(".view-tab")].find(tab=>tab.textContent==='+JSON.stringify(view)+').click()');
        await screenshot('index-collections-'+width+'-'+theme+'-'+density+'-'+showInspector+'-'+view);
        assert.equal(await evaluate('document.querySelectorAll(".trow.r-loadout,.card.r-loadout").length'),1);
        if(view==='TABLE')assert(await evaluate('(()=>{const row=document.querySelector(".trow.r-loadout");return row.querySelector(".cell-name").getBoundingClientRect().left-row.querySelector(".role-code").getBoundingClientRect().right>=10})()'));
        assert(await evaluate('document.documentElement.scrollWidth<=innerWidth&&document.querySelector(".topbar").getBoundingClientRect().top>=0'));
    }
    await evaluate('import("/js/theme.js").then(m=>m.updateSettings({theme:"dark",density:"Condensed",showInspector:true}))');
    await evaluate('[...document.querySelectorAll(".view-tab")].find(tab=>tab.textContent==="TABLE").click()');
    await navigate('/index?q=role:loadout');await wait('document.querySelectorAll(".trow").length===1');
    assert((await evaluate('document.querySelector(".trow").textContent')).includes('Qwen Image 2.1 collection'));
    await evaluate('document.querySelector(".trow").dispatchEvent(new MouseEvent("dblclick",{bubbles:true}))');
    await wait('!!document.querySelector(".drawer")');
    assert.equal(await evaluate('location.hash'),'#/loadout/'+collection.id);
    const timeOrigin=await evaluate('performance.timeOrigin');await send('Page.reload');
    await wait(`performance.timeOrigin!==${timeOrigin}&&document.querySelectorAll('.loadout-member').length===4`);
    await evaluate('document.querySelector("#loadout-close").click()');
    await navigate('/index?q='+encodeURIComponent(names[1][1]));
    await wait('document.querySelectorAll(".trow").length===2');
    await evaluate('[...document.querySelectorAll(".trow")].find(row=>row.textContent.includes("qwen3vl_8b")).click()');
    await wait('!!document.querySelector(".related-loadout")');
    await screenshot('related-collection');
    await evaluate('[...document.querySelectorAll(".trow")].find(row=>row.textContent.includes("qwen3vl_8b")).dispatchEvent(new MouseEvent("dblclick",{bubbles:true}))');
    await wait('!!document.querySelector(".detail-rail .related-loadout")');
    await screenshot('related-collection-detail');
    const detailHash=await evaluate('location.hash');
    await evaluate('document.querySelector(".detail-rail .related-loadout").click()');
    await wait('document.querySelectorAll(".loadout-member").length===4');
    await evaluate('document.querySelector("#loadout-close").click()');
    assert.equal(await evaluate('location.hash'),detailHash);
    await evaluate('document.querySelector(".detail-head .btn-back").click()');
    await wait('!!document.querySelector("#view-index:not([hidden]) .related-loadout")');

    await evaluate('document.querySelector(".related-loadout").click()');
    await wait('document.querySelectorAll(".loadout-member").length===4');
    await evaluate('document.querySelector("#loadout-delete").click();[...document.querySelectorAll(".loadout-delete-confirm button")].find(button=>button.textContent==="CANCEL").click()');
    assert.equal((await readApi('/loadouts')).length,1);
    await evaluate('document.querySelector("#loadout-delete").click()');
    await screenshot('delete-confirmation');
    await evaluate('document.querySelector(".loadout-delete-confirm button").click()');
    await wait('document.querySelector(".loadout-error").textContent.includes("token")');
    assert.equal((await readApi('/loadouts')).length,1);
    await evaluate('document.querySelector(".loadout-owner [name=owner_token]").value="synthetic-owner-token";document.querySelector(".loadout-owner form").requestSubmit();document.querySelector(".loadout-delete-confirm button").click()');
    await wait('document.querySelector(".loadout-editor").hidden');
    assert.equal((await readApi('/loadouts')).length,0);
    await evaluate('document.querySelector("#loadout-close").click()');
    await wait('!document.querySelector("#view-index .related-loadout")');
    assert.deepEqual((await readApi("/artifacts/"+ids[0])).loadouts,[]);
    for(const [folder,name] of names)assert.deepEqual(await readFile(join(fixtureRoot,folder,name)),fixtures.get(name));
    const originalAudit=join(scratch,'original-audit');await mkdir(originalAudit);
    const aliasBytes=Buffer.from(fixtures.values().next().value);aliasBytes[aliasBytes.length-1]=94;
    await writeFile(join(originalAudit,'a_original.safetensors'),aliasBytes);
    await writeApi('/sources',{name:'original-audit',path:originalAudit,kind:'loose'});
    let originalRow;
    for(let i=0;i<150;i++){originalRow=(await readApi('/search?q=source:original-audit')).resources[0];if(originalRow)break;await pause(100);}
    assert(originalRow);
    const auditRoot=join(scratch,'path-audit-models');
    for(const folder of ['loras','vae','text_encoders','diffusion_models/moss','diffusion_models/portrait'])await mkdir(join(auditRoot,folder),{recursive:true});
    await writeFile(join(auditRoot,'text_encoders/clip_alias_path_audit.safetensors'),aliasBytes);
    const auditFiles=new Map();
    for(const [index,path] of ['diffusion_models/moss/model.safetensors','diffusion_models/portrait/model.safetensors','diffusion_models/anchor.safetensors'].entries()){
        const bytes=Buffer.from(aliasBytes);bytes[bytes.length-1]=index+95;auditFiles.set(path,bytes);await writeFile(join(auditRoot,path),bytes);
    }
    await writeApi('/sources',{name:'paths-audit',path:auditRoot,kind:'loose'});
    let auditRows=[];
    for(let i=0;i<150;i++){auditRows=(await readApi('/search?q=source:paths-audit')).resources;if(auditRows.length===4)break;await pause(100);}
    assert.equal(auditRows.length,4);
    const alias=(await readApi('/search?q=source:paths-audit%20clip_alias_path_audit')).resources[0];
    assert.equal(alias.artifact_id,originalRow.artifact_id);
    assert.equal(alias.display_name,'clip_alias_path_audit.safetensors');
    assert.equal(alias.relative_path,'clip_alias_path_audit.safetensors');
    assert.equal(alias.name,'a_original.safetensors');
    await navigate('/index?q='+encodeURIComponent('source:paths-audit clip_alias_path_audit'));
    await wait('document.querySelectorAll(".trow").length===1&&document.querySelector(".trow .cell-name").textContent.includes("clip_alias_path_audit")');
    await screenshot('path-audit-alias-inspector');
    await wait('!!document.querySelector("#add-to-loadout")');
    await evaluate('window.auditFetch=window.fetch;window.auditPull="";window.fetch=(input,options)=>{if(String(input).includes("/pull-command"))window.auditPull=String(input);return window.auditFetch(input,options)};[...document.querySelectorAll(".inspector-actions button")].find(button=>button.textContent.includes("YANK")).click()');
    await wait('window.auditPull.includes("entry=")');
    assert.equal(new URL(await evaluate('window.auditPull'),origin).searchParams.get('entry'),alias.entry_id);
    await evaluate('window.fetch=window.auditFetch;document.querySelector(".trow").dispatchEvent(new MouseEvent("dblclick",{bubbles:true}))');
    await wait('document.querySelector(".file-row.is-opened")?.textContent.includes("clip_alias_path_audit")');
    assert.equal(new URLSearchParams((await evaluate('location.hash')).split('?')[1]).get('entry'),alias.entry_id);
    await send('Page.reload');
    await wait('document.querySelector(".file-row.is-opened")?.textContent.includes("clip_alias_path_audit")');
    assert((await evaluate('document.querySelector(".detail-rail").textContent')).includes('paths-audit/text_encoders'));
    await evaluate('document.querySelector(".detail-head .btn:not(.btn-back)").click()');
    await wait('!!document.querySelector("#loadout-new-name")');
    await evaluate('document.querySelector(".loadout-owner [name=owner_token]").value="synthetic-owner-token";document.querySelector(".loadout-owner form").requestSubmit();document.querySelector("#loadout-new-name").value="Selected local alias";document.querySelector(".drawer-new button").click()');
    await wait('document.querySelectorAll(".loadout-member").length===1&&!document.querySelector("#loadout-copy").disabled');
    const aliasCollection=(await readApi('/loadouts')).find(row=>row.name==='Selected local alias');
    const aliasDocument=await readApi('/loadouts/'+aliasCollection.id);
    assert.equal(aliasDocument.items[0].repo,'models/paths-audit/text_encoders');
    assert.equal(aliasDocument.items[0].path,'clip_alias_path_audit.safetensors');
    await evaluate('document.querySelector("#loadout-close").click()');
    await navigate('/index?q='+encodeURIComponent('source:paths-audit repo:diffusion_models'));
    await wait('document.querySelectorAll(".trow").length===3');
    const shown=await evaluate('[...document.querySelectorAll(".trow .cell-name")].map(row=>row.textContent)');
    assert(shown.some(name=>name.endsWith('moss/model.safetensors')));
    assert(shown.some(name=>name.endsWith('portrait/model.safetensors')));
    assert.deepEqual(shown,[...shown].sort());
    await screenshot('path-audit-nested-index');
    await evaluate('document.querySelectorAll(".trow").forEach(row=>{if(row.textContent.includes("moss/model")||row.textContent.includes("portrait/model")){row.click();window.dispatchEvent(new KeyboardEvent("keydown",{key:" ",bubbles:true}))}});document.querySelector("#add-to-loadout").click()');
    await wait('document.querySelector(".loadout-pending-label")?.textContent.includes("2 selected")');
    await evaluate('document.querySelector("#loadout-new").click();document.querySelector("#loadout-new-name").value="Nested path collection";document.querySelector(".drawer-new button").click()');
    await wait('document.querySelectorAll(".loadout-member").length===2&&!document.querySelector("#loadout-copy").disabled');
    await screenshot('path-audit-collection');
    const nestedCollection=(await readApi('/loadouts')).find(row=>row.name==='Nested path collection');
    const nestedDocument=await readApi('/loadouts/'+nestedCollection.id);
    assert.equal(new Set(nestedDocument.items.map(row=>row.destination)).size,2);
    const auditClient=join(scratch,'nested-client-models');
    const auditScript=await readApi('/loadouts/'+nestedCollection.id+'/download?'+new URLSearchParams({format:'shell',root:auditClient}));
    const auditScriptFile=join(scratch,'nested-download.sh');await writeFile(auditScriptFile,auditScript.text);
    await writeFile(join(scratch,'nested-download.log'),await runScript(auditScriptFile));
    for(const item of nestedDocument.items)assert.deepEqual(await readFile(join(auditClient,item.destination)),auditFiles.get(item.destination));
    await assert.rejects(access(join(auditClient,'diffusion_models/model.safetensors')));
    await unlink(join(auditRoot,'diffusion_models/anchor.safetensors'));
    const rescan=await writeApi('/scan',{source:'paths-audit',hash:'now'});
    for(let i=0;i<150;i++){if((await readApi('/jobs/'+rescan.job_id)).finished)break;await pause(100);}
    const retained=(await readApi('/search?q=source:paths-audit%20repo:diffusion_models')).resources;
    assert.equal(retained.length,2);assert(retained.every(row=>row.repo==='models/paths-audit/diffusion_models'));
    assert.deepEqual((await readApi('/loadouts/'+nestedCollection.id)).items.map(row=>row.destination),nestedDocument.items.map(row=>row.destination));
    await evaluate('document.querySelector("#loadout-close").click()');
    await writeApi('/loadouts/'+nestedCollection.id,undefined,'DELETE');
    await writeApi('/loadouts/'+aliasCollection.id,undefined,'DELETE');
    assert.equal(exceptions.length,0,JSON.stringify(exceptions));
    await writeFile(join(scratch,'verification.json'),JSON.stringify({passed:true,screenshots,checks:['four-file-Qwen-collection','nested-path-discovery-and-display','filename-alias-search','selected-entry-commands-and-reload','selected-entry-loadout-add','distinct-nested-client-destinations','category-repository-stability','twenty-model-condensed-capacity','collapsed-unsaved-path-notice','pending-rename-close-recovery','native-keyboard-disclosure','matching-control-heights','loadout-role-column-gap','two-encoders-retained','multi-selection-add','stable-shell-inputs-focus-scroll','explicit-rename-and-path-save','unsafe-path-recovery','real-hf-client-download','nested-source-preservation','only-selected-files','existing-client-files-preserved','server-originals-preserved','no-server-projections','search-by-name-and-member','loadout-role-filter','related-collection-discovery','deep-link-reload','related-collection-in-model-detail','32-index-layout-combinations','owner-access-recovery','remove-and-readd-member','unsaved-path-blocks-copy','powershell-script-save-utf8-bom','delete-cancel-and-confirm']},null,2));
    process.stdout.write(JSON.stringify({passed:true,scratch})+'\n');
} finally {
    closeBrowser();await stop(browser);await stop(server);
    await writeFile(join(scratch,'server.log'),serverLog);
}
