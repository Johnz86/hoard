import assert from 'node:assert/strict';
import { mkdtemp, mkdir, writeFile, readFile, access } from 'node:fs/promises';
import { spawn, spawnSync } from 'node:child_process';
import { tmpdir } from 'node:os';
import { join, dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import net from 'node:net';

const root = resolve(dirname(fileURLToPath(import.meta.url)), '..');
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
await writeFile(config, `[server]\nbind="127.0.0.1:${port}"\npublic_url="${origin}"\ndata_dir="${scratch}/data"\n[node]\nname="first-run"\ndiscovery="none"\n[index]\nhash_policy="never"\nhash_threads=2\nwatch=true\nreconcile_interval_secs=0\n[mirror]\noffline=true\n`);
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
    browser = spawn('nice',['-n','10','/usr/bin/brave-browser','--headless=new','--no-sandbox',
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
    const press=async(key,code=key)=>{await send('Input.dispatchKeyEvent',{type:'keyDown',key,code});await send('Input.dispatchKeyEvent',{type:'keyUp',key,code});};
    const navigate=async hash=>evaluate('location.hash='+JSON.stringify(hash));
    const screenshot=async name=>{await evaluate('document.fonts.ready');const shot=await send('Page.captureScreenshot',{format:'png'});await writeFile(join(scratch,name+'.png'),Buffer.from(shot.data,'base64'));};
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
    await evaluate('document.querySelector("[name=public_url]").value='+JSON.stringify(origin)+';document.querySelector(".sharing-form").requestSubmit()');
    await wait('document.querySelector(".sharing-status").textContent.includes("Restart")');
    const savedSetup=await(await fetch(origin+'/api/v1/setup')).json();assert.equal(savedSetup.node_name,'first-run');assert.equal(savedSetup.pending.node_name,'Studio');
    await evaluate('document.querySelector(".operations-top button").click()');
    await evaluate('document.querySelector("#source-path").value='+JSON.stringify(source)+';document.querySelector("[name=name]").value="projects";document.querySelector("[name=kind]").value="git";document.querySelector(".source-add-form").requestSubmit()');
    await wait('document.querySelectorAll(".source-grid.panel-row").length===2');
    await wait('!document.querySelector("#source-welcome")');
    await navigate('/index?q=pose');await wait('document.querySelectorAll(".trow").length===1');
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
    for(const name of ['projects','empty']) {
        await wait('!!document.querySelector("button[data-source='+name+'][data-action=remove]")&&!document.querySelector("button[data-source='+name+'][data-action=remove]").disabled');
        await evaluate('document.querySelector("button[data-source='+name+'][data-action=remove]").click();document.querySelector("button[data-source='+name+'][data-action=confirm]").click()');
        await wait('!document.querySelector("button[data-source='+name+']")');
    }
    await wait('!!document.querySelector("#source-welcome")');
    assert.equal((await(await fetch(origin+'/api/v1/setup')).json()).first_run,true);
    await access(join(source,'README.md'));
    assert.equal(exceptions.length,0,JSON.stringify(exceptions));
    await writeFile(join(scratch,'verification.json'),JSON.stringify({passed:true,checks:['fresh-route','explicit-empty-query','empty-catalog-action','owner-access','invalid-path-recovery','empty-folder','sharing-form-polling-focus','folder-form-position-during-background-indexing','URL-validation','pending-vs-active','project-add-and-review','deep-link-reload','token-not-persisted','restart-config-and-resources','existing-node-index','remove-last-source'],screenshots:4},null,2));
    process.stdout.write(JSON.stringify({passed:true,scratch})+'\n');
} finally {
    socket?.close();await stop(browser);await stop(server);
    await writeFile(join(scratch,'server.log'),serverLog);
}
