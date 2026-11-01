import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, access, unlink, utimes } from 'node:fs/promises';
import { spawn, spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '../..');
const scratch = await mkdtemp(join(tmpdir(), 'hoard-first-run-'));
const source = join(scratch, 'pose-project');
const env = { ...process.env, GIT_CONFIG_GLOBAL: '/dev/null', GIT_CONFIG_SYSTEM: '/dev/null',
    RAYON_NUM_THREADS: '2', TOKIO_WORKER_THREADS: '2', GIT_LFS_SKIP_SMUDGE: '1',
    HOARD_NODE_TOKEN: 'synthetic-owner-token' };
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
        socket?.close();
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
    assert.equal((await(await fetch(origin+'/api/v1/setup')).json()).first_run,true);
    for(const width of [1600,1180]) for(const theme of ['light','dark']) {
        await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:false});
        await evaluate('import("/js/theme.js").then(m=>m.updateSettings('+JSON.stringify({theme})+'))');
        await pause(350);
        await screenshot('fresh-'+theme+'-'+width);
        assert(!(await evaluate('document.documentElement.scrollWidth>innerWidth')));
        assert(await evaluate('[...document.querySelectorAll(".setup-steps button")].every(b=>b.getBoundingClientRect().right<=innerWidth)'));
    }
    await send('Page.navigate',{url:origin+'/#/index?q=missing'});
    await wait('document.querySelector(".catalog-empty")?.textContent.includes("CLEAR SEARCH")');
    assert.equal(await evaluate('location.hash'),'#/index?q=missing');
    await evaluate('document.querySelector(".setup-empty-actions button").click()');
    await wait('document.querySelector(".catalog-empty")?.textContent.includes("YOUR CATALOG IS EMPTY")');
    await evaluate('document.querySelector(".setup-empty-actions button").click()');
    await wait('!!document.querySelector(".source-add-form")');
    assert.equal(await evaluate('document.activeElement?.id'),'source-path');
    await evaluate('document.querySelector("#source-path").value='+JSON.stringify(missingPath)+';document.querySelector("[name=name]").value="example";document.querySelector(".source-add-form").requestSubmit()');
    await wait('!document.querySelector(".source-form-error").hidden');
    assert(await evaluate('document.querySelector(".source-form-error").textContent.includes("Owner access")'));
    assert(await evaluate('document.querySelector("#source-sharing").open'));
    await evaluate('document.querySelector("[name=owner_token]").value="synthetic-owner-token";document.querySelector(".owner-access").requestSubmit()');
    assert(!(await evaluate('document.querySelector("#source-sharing summary").textContent.includes("OWNER ACCESS REQUIRED")')));
    await evaluate('document.querySelector(".source-add-form").requestSubmit()');
    await wait('!document.querySelector(".source-add-form button[type=submit]").disabled');
    assert.equal(await evaluate('document.querySelector("#source-path").value'),missingPath);
    assert(!(await evaluate('document.querySelector(".source-form-error").textContent.includes("Owner access")')));
    await evaluate('document.querySelector("#source-path").value='+JSON.stringify(empty)+';document.querySelector("[name=name]").value="empty";document.querySelector(".source-add-form").requestSubmit()');
    await wait('document.querySelectorAll(".source-grid.panel-row").length===1');
    await wait('document.querySelector("#source-folders").textContent.includes("No files found")');
    assert(await evaluate('!!document.querySelector("#source-welcome")'));
    await evaluate('document.querySelector(".operations-top button").click();document.querySelector("#source-path").value="/draft-folder";document.querySelector("#source-path").focus()');
    const formTop=await evaluate('document.querySelector("#source-path").getBoundingClientRect().top');
    const addedInBackground=await fetch(origin+'/api/v1/sources',{method:'POST',headers:{'content-type':'application/json',authorization:'Bearer synthetic-owner-token'},body:JSON.stringify({name:'background-project',path:source,kind:'git'})});
    assert(addedInBackground.ok,await addedInBackground.text());
    await wait('document.querySelectorAll(".source-grid.panel-row").length===2');
    await pause(5500);
    assert(await evaluate('!!document.querySelector("#source-welcome")'));
    assert.equal(await evaluate('document.querySelector("#source-path").value'),'/draft-folder');
    assert.equal(await evaluate('document.activeElement?.id'),'source-path');
    assert.equal(await evaluate('document.querySelector("#source-path").getBoundingClientRect().top'),formTop);
    await press('Escape','Escape');await press('Escape','Escape');
    await wait('!document.querySelector(".source-add-form")&&!document.querySelector("#source-welcome")');
    const removedInBackground=await fetch(origin+'/api/v1/sources/background-project',{method:'DELETE',headers:{authorization:'Bearer synthetic-owner-token'}});assert(removedInBackground.ok);
    await wait('!!document.querySelector("#source-welcome")');
    await navigate('/index');
    await wait('document.querySelector(".catalog-empty")?.textContent.includes("Check Activity")');
    await navigate('/sources');await wait('!!document.querySelector(".sharing-form")');
    await evaluate('document.querySelector("#source-sharing").open=true;const name=document.querySelector("[name=node_name]");name.value="Studio";name.focus()');
    await pause(11000);
    assert.equal(await evaluate('document.querySelector("[name=node_name]").value'),'Studio');
    assert.equal(await evaluate('document.activeElement?.name'),'node_name');
    await evaluate('document.querySelector("[name=public_url]").value="http://0.0.0.0:7099";document.querySelector(".sharing-form").requestSubmit()');
    await wait('!document.querySelector(".sharing-form button[type=submit]").disabled');
    assert.equal((await(await fetch(origin+'/api/v1/setup')).json()).restart_required,false);
    const liveOrigin=origin.replace('127.0.0.1','localhost');
    const servingPid=server.pid;
    await evaluate('document.querySelector("[name=node_name]").value="first-run";document.querySelector("[name=public_url]").value='+JSON.stringify(liveOrigin)+';document.querySelector(".sharing-form").requestSubmit()');
    await wait('document.querySelector(".sharing-address").textContent.includes('+JSON.stringify(liveOrigin)+')&&!document.querySelector(".sharing-form button[type=submit]").disabled');
    const liveSetup=await(await fetch(origin+'/api/v1/setup')).json();
    assert.equal(liveSetup.public_url,liveOrigin);assert.equal(liveSetup.pending,null);assert.equal(liveSetup.restart_required,false);
    assert.equal(liveSetup.bind,'127.0.0.1:'+port);assert.equal(server.pid,servingPid);assert.equal(server.exitCode,null);
    assert((await fetch(liveOrigin+'/healthz')).ok);assert((await fetch(origin+'/healthz')).ok);
    for(const width of [1180,1600])for(const theme of ['light','dark']){
        await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:false});
        await evaluate('import("/js/theme.js").then(m=>m.updateSettings('+JSON.stringify({theme})+'))');
        await pause(250);await screenshot('live-sharing-'+width+'-'+theme);
    }
    await evaluate('document.querySelector("[name=node_name]").value="Studio"');
    await evaluate('document.querySelector("[name=public_url]").value='+JSON.stringify(origin)+';document.querySelector(".sharing-form").requestSubmit()');
    await wait('document.querySelector(".sharing-status").textContent.includes("Restart")');
    const savedSetup=await(await fetch(origin+'/api/v1/setup')).json();assert.equal(savedSetup.node_name,'first-run');assert.equal(savedSetup.pending.node_name,'Studio');assert.equal(savedSetup.public_url,origin);
    await evaluate('document.querySelector(".operations-top button").click()');
    await evaluate('document.querySelector("#source-path").value='+JSON.stringify(source)+';document.querySelector("[name=name]").value="projects";document.querySelector("[name=kind]").value="git";document.querySelector(".source-add-form").requestSubmit()');
    await wait('document.querySelectorAll(".source-grid.panel-row").length===2');
    await wait('!document.querySelector("#source-welcome")');
    await navigate('/index?q=pose');await wait('document.querySelectorAll(".trow").length===1');
    await navigate('/sources');await wait('!!document.querySelector(".sharing-form")');
    await evaluate('document.querySelector("#source-sharing").open=true;document.querySelector("[name=public_url]").value='+JSON.stringify(liveOrigin)+';document.querySelector(".sharing-form").requestSubmit()');
    await wait('document.querySelector(".sharing-address").textContent.includes('+JSON.stringify(liveOrigin)+')&&!document.querySelector(".sharing-form button[type=submit]").disabled');
    await navigate('/index?q=pose');await wait('document.querySelector(".trow")?.textContent.includes("pose")');
    await evaluate('document.querySelector(".inspector-actions button:nth-child(2)").click()');
    await wait('document.querySelector("#toast-root").textContent.includes('+JSON.stringify(liveOrigin)+')');
    await evaluate('document.querySelector(".trow").dispatchEvent(new MouseEvent("dblclick",{bubbles:true}))');
    await wait('document.querySelector(".project-document")?.textContent.includes("animation toolkit")');
    const projectHash=await evaluate('location.hash');
    const delayedSetup = await send('Page.addScriptToEvaluateOnNewDocument',{source:'const normalFetch=window.fetch.bind(window);window.fetch=(input,options)=>String(input).endsWith("/api/v1/setup")?new Promise(()=>{}):normalFetch(input,options)'});
    await send('Page.reload');await wait('document.querySelector(".project-document")?.textContent.includes("animation toolkit")');
    assert.equal(await evaluate('location.hash'),projectHash);
    await send('Page.removeScriptToEvaluateOnNewDocument',{identifier:delayedSetup.identifier});
    await send('Page.reload');await wait('document.querySelector(".project-document")?.textContent.includes("animation toolkit")');
    await navigate('/sources');await wait('!!document.querySelector(".owner-access")');
    assert(!(await evaluate('document.querySelector(".owner-access").textContent.includes("Token set for this tab")')));
    assert(!(await evaluate('JSON.stringify(localStorage).includes("synthetic-owner-token")')));
    await stop(server);await startServer();
    const restarted=await(await fetch(origin+'/api/v1/setup')).json();assert.equal(restarted.node_name,'Studio');assert.equal(restarted.restart_required,false);assert.equal(restarted.resource_count,1);
    await send('Page.navigate',{url:origin+'/'});await wait('document.querySelectorAll(".trow").length===1');
    assert(!(await evaluate('!!document.querySelector("#source-welcome")')));
    await navigate('/sources');await wait('!!document.querySelector(".owner-access")');
    await evaluate('document.querySelector("#source-sharing").open=true;document.querySelector("[name=owner_token]").value="synthetic-owner-token";document.querySelector(".owner-access").requestSubmit()');
    const models=join(scratch,'models');await mkdir(models);
    const modelHeader=Buffer.from(JSON.stringify({__metadata__:{ss_network_dim:'16',ss_network_alpha:'8',ss_base_model_version:'sdxl_base_v1-0'},
        'lora_unet_layer.lora_down.weight':{dtype:'F16',shape:[16,8],data_offsets:[0,256]},
        'lora_unet_layer.lora_up.weight':{dtype:'F16',shape:[8,16],data_offsets:[256,512]}}));
    const modelSize=Buffer.alloc(8);modelSize.writeBigUInt64LE(BigInt(modelHeader.length));
    const modelBytes=Buffer.concat([modelSize,modelHeader,Buffer.alloc(512)]);
    await writeFile(join(models,'portrait-light.safetensors'),modelBytes);
    const mysteryHeader=Buffer.from(JSON.stringify({'mystery.weight':{dtype:'F16',shape:[2,2],data_offsets:[0,8]}}));
    const mysterySize=Buffer.alloc(8);mysterySize.writeBigUInt64LE(BigInt(mysteryHeader.length));
    const mysteryPath=join(models,'mystery.safetensors');
    const mysteryBytes=Buffer.concat([mysterySize,mysteryHeader,Buffer.alloc(8)]);
    await writeFile(mysteryPath,mysteryBytes);
    const modelSource=await fetch(origin+'/api/v1/sources',{method:'POST',headers:{'content-type':'application/json',authorization:'Bearer synthetic-owner-token'},body:JSON.stringify({name:'models',path:models,kind:'loose'})});assert(modelSource.ok);
    for(let i=0;i<150;i++){if((await(await fetch(origin+'/api/v1/search?q=role:lora')).json()).resources.length)break;await pause(100);}
    await navigate('/index?q=role:lora');await wait('document.querySelectorAll(".trow").length===1&&document.querySelector(".trow").textContent.includes("portrait-light")');
    await wait('!!document.querySelector(".classification-review")');
    await evaluate('document.querySelector(".classification-review").open=true;document.querySelector(".classification-evidence").open=true');
    assert(await evaluate('document.querySelectorAll(".classification-evidence-row").length>0'));
    assert.equal(await evaluate('document.querySelector("#classification-role").value'),'lora');
    await press('Escape','Escape');
    await evaluate('document.querySelector("#add-to-loadout").focus();document.querySelector("#add-to-loadout").click()');
    await wait('!!document.querySelector("#loadout-new-name")');
    await evaluate('document.querySelector("#loadout-new-name").value="Portrait collection";document.querySelector(".drawer-new button").click()');
    await wait('document.querySelectorAll(".loadout-member").length===1&&!document.querySelector("#loadout-copy").disabled');
    await screenshot('loadout-client-download');
    await evaluate('window.drawerIdentity=document.querySelector(".drawer");document.querySelector("#loadout-name").value="Unsaved collection draft";document.querySelector("#loadout-name").dispatchEvent(new Event("input"));document.querySelector("#loadout-name").focus();document.querySelector("#loadout-name").setSelectionRange(1,5)');
    await pause(1500);
    assert(await evaluate('window.drawerIdentity===document.querySelector(".drawer")'));
    assert.equal(await evaluate('document.querySelector("#loadout-name").value'),'Unsaved collection draft');
    assert.equal(await evaluate('document.activeElement.id'),'loadout-name');
    await evaluate('document.querySelector("#loadout-close").click()');
    await wait('!!document.querySelector("#add-to-loadout")');
    assert.equal(await evaluate('document.activeElement.id'),'add-to-loadout');
    const readApi=async path=>(await fetch(origin+'/api/v1'+path)).json();
    const assertTriage=async count=>{
        const [stats,queue,search,shelves]=await Promise.all(['/stats','/triage','/search?q=state:triage','/shelves'].map(readApi));
        assert.equal(stats.triage,count);assert.equal(queue.length,count);assert.equal(search.total,count);
        assert.equal(shelves.find(s=>s.query==='state:triage').count,count);
    };
    const scanModels=async()=>{
        const response=await fetch(origin+'/api/v1/scan',{method:'POST',headers:{'content-type':'application/json',authorization:'Bearer synthetic-owner-token'},body:JSON.stringify({source:'models',hash:'now'})});assert(response.ok);
        const {job_id}=await response.json();
        for(let i=0;i<150;i++){const job=await readApi('/jobs/'+job_id);if(job.finished){assert.equal(job.state,'done',JSON.stringify(job));return;}await pause(100);}
        throw new Error('Model scan did not finish');
    };
    await navigate('/index?q=state:triage');
    await wait('document.querySelector(".trow")?.textContent.includes("mystery")&&!!document.querySelector(".classification-review")');
    await assertTriage(1);
    const mysteryId=(await readApi('/search?q=state:triage')).resources[0].artifact_id;
    await send('Page.reload');
    await wait('!!document.querySelector(".classification-review")');
    await evaluate('document.querySelector(".classification-review > summary").focus()');await press('Enter','Enter');
    await wait('document.querySelector(".classification-review").open');
    assert(await evaluate('document.querySelector(".classification-review").textContent.includes("Role is unclassified")'));
    await evaluate('const role=document.querySelector("#classification-role");role.value="ctrl";role.dispatchEvent(new Event("change"));const base=document.querySelector("#classification-base");base.value="sdxl";base.dispatchEvent(new Event("input"));base.focus();base.setSelectionRange(1,3)');
    await evaluate('document.querySelector("#loadout-btn").click();document.querySelector(".drawer-head button").click()');
    assert.equal(await evaluate('document.activeElement?.id'),'classification-base');
    assert.deepEqual(await evaluate('[document.activeElement.selectionStart,document.activeElement.selectionEnd]'),[1,3]);
    await evaluate('document.querySelector(".classification-review form").requestSubmit()');
    await wait('document.querySelector(".classification-review").textContent.includes("Owner access is required")');
    assert.equal(await evaluate('document.querySelector("#classification-base").value'),'sdxl');
    await assertTriage(1);
    await evaluate('document.querySelector(".classification-review a").click()');
    await wait('!!document.querySelector(".owner-access")');
    await evaluate('document.querySelector("#source-sharing").open=true;document.querySelector("[name=owner_token]").value="synthetic-owner-token";document.querySelector(".owner-access").requestSubmit()');
    await navigate('/index?q=state:triage');await wait('!!document.querySelector("#classification-base")');
    assert.equal(await evaluate('document.querySelector("#classification-base").value'),'sdxl');
    assert.equal(await evaluate('document.querySelector("#classification-role").value'),'ctrl');
    for(const width of [1180,1600])for(const theme of ['light','dark'])
    for(const density of ['Condensed','Roomy'])for(const showInspector of [true,false])
    for(const view of ['TABLE','CARDS']){
        await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:false});
        await evaluate('import("/js/theme.js").then(m=>m.updateSettings('+JSON.stringify({theme,density,showInspector})+'))');
        await evaluate('[...document.querySelectorAll(".view-tab")].find(e=>e.textContent==='+JSON.stringify(view)+').click()');
        if(showInspector)await evaluate('{const rail=document.querySelector(".inspector");rail.scrollTop+=document.querySelector(".classification-review").getBoundingClientRect().top-rail.getBoundingClientRect().top-16}');
        await pause(250);
        await screenshot('classification-review-'+width+'-'+theme+'-'+density+'-'+showInspector+'-'+view);
        assert(await evaluate('document.querySelector(".shell").scrollTop===0&&document.querySelector(".topbar").getBoundingClientRect().top>=0'));
        if(showInspector){
            assert(!(await evaluate('document.querySelector(".inspector").scrollWidth>document.querySelector(".inspector").clientWidth')));
            assert(await evaluate('document.querySelector(".inspector").getBoundingClientRect().bottom<=innerHeight'));
        }
    }
    await evaluate('import("/js/theme.js").then(m=>m.updateSettings({theme:"dark",density:"Condensed",showInspector:true}))');
    await evaluate('[...document.querySelectorAll(".view-tab")].find(e=>e.textContent==="TABLE").click()');
    await evaluate('document.querySelector("#classification-base").focus()');await press('Enter','Enter');
    await wait('document.querySelector(".catalog-empty")?.textContent.includes("NO RESOURCE MATCHES")');
    await assertTriage(0);
    await navigate('/index?q=mystery');await wait('!!document.querySelector("#classification-clear")');
    assert.equal((await readApi('/artifacts/'+mysteryId)).role,'ctrl');
    assert.equal(await evaluate('document.querySelector("#classification-role").value'),'ctrl');
    assert.equal(await evaluate('document.querySelector("#classification-base").value'),'sdxl');
    const overrideTime=new Date(Date.now()+1000);await utimes(mysteryPath,overrideTime,overrideTime);
    await scanModels();await assertTriage(0);
    assert.equal((await readApi('/artifacts/'+mysteryId)).role,'ctrl');
    await stop(server);await startServer();await send('Page.reload');
    await wait('!!document.querySelector("#classification-clear")');
    const persisted=await readApi('/artifacts/'+mysteryId);assert(persisted.override_applied);assert.equal(persisted.role,'ctrl');assert.equal(persisted.base,'sdxl');
    await navigate('/sources');await wait('!!document.querySelector(".owner-access")');
    await evaluate('document.querySelector("#source-sharing").open=true;document.querySelector("[name=owner_token]").value="synthetic-owner-token";document.querySelector(".owner-access").requestSubmit()');
    await navigate('/index?q=mystery');await wait('!!document.querySelector("#classification-clear")');
    await evaluate('document.querySelector(".classification-review").open=true;const role=document.querySelector("#classification-role");role.value="llm";role.dispatchEvent(new Event("change"));const base=document.querySelector("#classification-base");base.value="qwen";base.dispatchEvent(new Event("input"));document.querySelector(".classification-review form").requestSubmit()');
    await wait('document.querySelector(".trow .role-code")?.textContent==="LLM"&&!!document.querySelector("#classification-clear")');
    const corrected=await readApi('/artifacts/'+mysteryId);assert.equal(corrected.role,'llm');assert.equal(corrected.base,'qwen');
    assert(await evaluate('document.querySelector(".inspector-name").textContent.includes("mystery")'));
    await assertTriage(0);
    await evaluate('document.querySelector(".classification-review").open=true;document.querySelector("#classification-clear").click()');
    await wait('!!document.querySelector(".classification-review")&&!document.querySelector("#classification-clear")');
    assert.equal((await readApi('/artifacts/'+mysteryId)).override_applied,false);
    const changedTime=new Date(Date.now()+2000);await utimes(mysteryPath,changedTime,changedTime);await scanModels();
    await navigate('/index?q=state:triage');await wait('document.querySelector(".trow")?.textContent.includes("mystery")&&document.querySelector(".cell-state")?.textContent==="TRIAGE"&&!!document.querySelector(".classification-review")');
    await assertTriage(1);
    await evaluate('document.querySelector(".classification-review").open=true;document.querySelector("#classification-reject").click()');
    await wait('document.querySelector("#classification-reject")?.textContent==="CONFIRM REJECT"');
    assert(await evaluate('document.querySelector(".classification-review").textContent.includes("Files stay on disk")'));
    await evaluate('[...document.querySelectorAll(".classification-review button")].find(b=>b.textContent==="CANCEL").click()');
    assert.equal((await readApi('/triage/noise')).length,0);await assertTriage(1);
    await evaluate('document.querySelector("#classification-base").focus()');await press('Escape','Escape');
    assert(!(await evaluate('document.querySelector(".classification-review").open')));
    assert(await evaluate('document.activeElement===document.querySelector(".classification-review > summary")'));
    await press('Enter','Enter');
    await evaluate('document.querySelector("#classification-reject").click();document.querySelector("#classification-reject").click()');
    await wait('document.querySelector(".catalog-empty")?.textContent.includes("NO RESOURCE MATCHES")');
    await assertTriage(0);assert((await readApi('/triage/noise')).length>0);
    await scanModels();assert.equal((await readApi('/search?q=mystery')).total,0);
    assert.deepEqual(await readFile(mysteryPath),mysteryBytes);
    await stop(server);await startServer();await send('Page.reload');await assertTriage(0);
    assert.equal((await readApi('/search?q=mystery')).total,0);
    await navigate('/index?q=role:git');await wait('document.querySelector(".trow")?.textContent.includes("pose-project")');
    assert(!(await evaluate('!!document.querySelector(".classification-review")')));
    await navigate('/sources');await wait('!!document.querySelector(".owner-access")');
    await evaluate('document.querySelector("#source-sharing").open=true;document.querySelector("[name=owner_token]").value="synthetic-owner-token";document.querySelector(".owner-access").requestSubmit()');
    await wait('!!document.querySelector("button[data-source=models][data-action=remove]")');
    await evaluate('document.querySelector("button[data-source=models][data-action=remove]").click();document.querySelector("button[data-source=models][data-action=confirm]").click()');
    await wait('!document.querySelector("button[data-source=models]")');
    for(const name of ['projects','empty']) {
        await wait('!!document.querySelector("button[data-source='+name+'][data-action=remove]")&&!document.querySelector("button[data-source='+name+'][data-action=remove]").disabled');
        await evaluate('document.querySelector("button[data-source='+name+'][data-action=remove]").click();document.querySelector("button[data-source='+name+'][data-action=confirm]").click()');
        await wait('!document.querySelector("button[data-source='+name+']")');
    }
    await wait('!!document.querySelector("#source-welcome")');
    assert.equal((await(await fetch(origin+'/api/v1/setup')).json()).first_run,true);
    await access(join(source,'README.md'));
    assert.equal(exceptions.length,0,JSON.stringify(exceptions));
    await writeFile(join(scratch,'verification.json'),JSON.stringify({passed:true,checks:['fresh-route','explicit-empty-query','empty-catalog-action','owner-access','invalid-path-recovery','empty-folder','sharing-form-polling-focus','folder-form-position-during-background-indexing','URL-validation','live-sharing-address-without-restart','listener-and-server-preserved','live-cached-clone-command','pending-vs-active','project-add-and-review','deep-link-reload','token-not-persisted','restart-config-and-resources','existing-node-index','loadout-client-script','loadout-stable-drawer','loadout-input-focus','classification-owner-recovery','classification-draft-focus','classification-confirm-counts','classification-rescan-restart','classification-clear-and-reinfer','classification-reject-cancel','classification-noise-preserved','project-without-model-controls','remove-last-source'],screenshots},null,2));
    process.stdout.write(JSON.stringify({passed:true,scratch})+'\n');
} finally {
    socket?.close();await stop(browser);await stop(server);
    await writeFile(join(scratch,'server.log'),serverLog);
}
