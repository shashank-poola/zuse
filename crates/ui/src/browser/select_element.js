// The native host pulls one committed note at a time from a persistent session.
// Each mark owns its own editor; subsequent pulls never recreate text fields.
const generation = typeof zuseAnnotationGeneration === 'undefined' ? 'fixture' : String(zuseAnnotationGeneration);
const palette = typeof zuseAnnotationTheme === 'object' ? zuseAnnotationTheme : {};
const displayScale = typeof zuseAnnotationDisplayScale === 'number' ? zuseAnnotationDisplayScale : devicePixelRatio;
if (globalThis.zuseAnnotationSession?.generation === generation) {
  globalThis.zuseAnnotationSession.configure(palette, displayScale);
  return await globalThis.zuseAnnotationSession.next();
}
if (globalThis.zuseCancelSelection) globalThis.zuseCancelSelection();
const host = document.createElement('div');
host.dataset.zuseAnnotation = '';
host.setAttribute('role', 'group');
Object.assign(host.style, {all:'initial',position:'fixed',inset:'0',zIndex:'2147483647',pointerEvents:'none'});
const root = host.attachShadow({mode:'closed'});
const css = `
  :host {font:12px/1.4 var(--font,system-ui,sans-serif);text-align:left;color-scheme:normal}
  * {box-sizing:border-box} [hidden] {display:none!important}
  button {all:unset;box-sizing:border-box;display:inline-flex;align-items:center;justify-content:center;flex-shrink:0;height:28px;padding:0 8px;border-radius:6px;font:500 11px/1 var(--font,system-ui,sans-serif);white-space:nowrap;cursor:pointer;user-select:none}
  button:focus-visible {outline:2px solid var(--accent);outline-offset:2px}
  .tools,.panel {font:12px/1.4 var(--font,system-ui,sans-serif);text-align:left;color:var(--fg);background:var(--surface);border:1px solid var(--border);box-shadow:0 4px 20px var(--shadow);pointer-events:auto}
  .tools {position:fixed;top:10px;left:50%;transform:translateX(-50%);z-index:30;display:flex;align-items:center;justify-content:center;gap:6px;flex-wrap:wrap;width:max-content;padding:6px;border-radius:10px}
  .tool-group {display:flex;align-items:center;gap:2px}.tool-group + .tool-group {border-left:1px solid var(--border);padding-left:6px}
  .tools .icon-button {width:28px;padding:0}.tools svg {width:15px;height:15px;pointer-events:none}
  .tools button {color:var(--muted)}.tools button:hover:not(:disabled) {background:var(--hover);color:var(--fg)}.tools button[aria-pressed=true] {color:var(--on-accent,var(--on-primary));background:var(--accent-fill,var(--accent))}button:disabled {opacity:.4;cursor:default}
  .tools .done {color:var(--on-primary);background:var(--primary);padding:0 10px}.tools .done:hover {color:var(--on-primary);background:var(--primary);opacity:.9}
  .shapes {position:fixed;inset:0;width:100%;height:100%;pointer-events:none;overflow:visible}
  .outline {position:fixed;border:1px solid var(--accent);background:var(--hover-wash);pointer-events:none;border-radius:3px}
  .pin {position:fixed;z-index:20;min-width:22px;height:22px;padding:0;border-radius:11px;color:var(--on-primary);background:var(--primary);border:2px solid var(--surface);box-shadow:0 1px 5px var(--shadow);pointer-events:auto;font-weight:600;gap:4px}
  .pin-number {min-width:18px;text-align:center}.pin-tag {max-width:96px;overflow:hidden;text-overflow:ellipsis;padding-right:6px;font-weight:500}
  .pin[aria-pressed=true] {outline:2px solid var(--accent);outline-offset:1px}
  .target-label {position:fixed;z-index:20;max-width:160px;padding:2px 6px;border-radius:4px;background:var(--accent-fill,var(--accent));color:var(--on-accent,var(--on-primary));font:500 11px/16px var(--font,system-ui,sans-serif);overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  .panel {position:relative;display:flex;flex-direction:column;width:100%;max-height:inherit;padding:8px;border-radius:10px;overflow-y:auto;overscroll-behavior:contain}
  .heading {display:flex;align-items:center;flex-shrink:0;gap:6px;min-height:22px;margin-bottom:4px;font-size:11px;color:var(--muted)}
  .number {display:inline-flex;align-items:center;justify-content:center;min-width:18px;height:18px;border-radius:5px;background:var(--hover);font-weight:600;color:var(--fg)}.anchor {flex:1;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
  .close {width:22px;height:22px;padding:0;color:var(--muted);background:transparent;font-size:18px;font-weight:400}.close:hover {color:var(--fg);background:var(--hover)}
  textarea {all:unset;box-sizing:border-box;display:block;width:100%;height:44px;min-height:36px;max-height:108px;padding:4px 2px;overflow-y:auto;resize:none;background:transparent;color:var(--fg);caret-color:var(--fg);font:12px/18px var(--font,system-ui,sans-serif);white-space:pre-wrap;overflow-wrap:break-word;cursor:text;user-select:text;-webkit-user-select:text}
  textarea::placeholder {color:var(--muted)}
  .actions {display:flex;align-items:center;justify-content:space-between;flex-wrap:wrap;flex-shrink:0;gap:8px;margin-top:6px;padding-top:6px;border-top:1px solid var(--border)}
  .status {margin-right:auto;font-size:10px;color:var(--muted);white-space:nowrap}
  .add {background:var(--primary);color:var(--on-primary)}.add:hover:not(:disabled) {opacity:.9}
`;
const sheet = document.createElement('style'); sheet.textContent = css;
const svg = document.createElementNS('http://www.w3.org/2000/svg','svg'); svg.setAttribute('class','shapes');
const hover = document.createElement('div'); hover.className = 'outline';
const hoverLabel = document.createElement('div'); hoverLabel.className = 'target-label';
const pins = document.createElement('div');
const tools = document.createElement('div'); tools.className = 'tools'; tools.setAttribute('role','toolbar'); tools.setAttribute('aria-label','Annotation tools');
const slot = document.createElement('slot');
root.append(sheet,svg,hover,hoverLabel,pins,tools,slot);
document.documentElement.append(host);
let notes = [], history = [], drag = null, hovered = null, activeId = null, mode = 'select', sequence = 0;
let closed = false, waiter = null, queue = [], scale = displayScale, focusFrame = null;
const buttons = new Map();
// Keep tools and history in separate groups, like the native screenshot bar.
function iconButton(label, path) {
  const button=document.createElement('button');button.className='icon-button';button.title=label;button.setAttribute('aria-label',label);
  const icon=document.createElementNS(svg.namespaceURI,'svg');icon.setAttribute('viewBox','0 0 24 24');icon.setAttribute('fill','none');icon.setAttribute('stroke','currentColor');icon.setAttribute('stroke-width','1.7');icon.setAttribute('stroke-linecap','round');icon.setAttribute('stroke-linejoin','round');icon.setAttribute('aria-hidden','true');
  const shape=document.createElementNS(svg.namespaceURI,'path');shape.setAttribute('d',path);icon.append(shape);button.append(icon);return button;
}
const toolIcons={select:'M4 3v16l4-4 3 6 3-1-3-6h6L4 3Z',region:'M8 4H4v4m12-4h4v4M4 16v4h4m12-4v4h-4M8 4h1m6 0h1M4 11v2m16-2v2M11 20h2',draw:'m4 16-1 5 5-1L20 8a2.8 2.8 0 0 0-4-4L4 16Zm10-10 4 4M4 16l4 4',erase:'m14 4 6 6a2 2 0 0 1 0 3l-8 8H7l-4-4a2 2 0 0 1 0-3l10-10a1 1 0 0 1 1 0ZM8 9l9 9m-5 3h9'};
const modes=document.createElement('div');modes.className='tool-group';
const historyTools=document.createElement('div');historyTools.className='tool-group';
for (const [id,label] of [['select','Select'],['region','Region'],['draw','Draw'],['erase','Erase']]) {
  const button = iconButton(label,toolIcons[id]);button.title=`${label} (${buttons.size+1})`;
  button.addEventListener('click',()=>setTool(id)); modes.append(button); buttons.set(id,button);
}
const undo = iconButton('Undo annotation','M9 4 4 9l5 5M4 9h9a6 6 0 0 1 0 12');
const clear = iconButton('Clear annotation targets','M3 6h18M9 6V3h6v3M5 6l1 15h12l1-15M10 10v7m4-7v7');
const done = document.createElement('button'); done.className='done';done.textContent = 'Done'; done.setAttribute('aria-label','Finish annotating');
historyTools.append(undo,clear);tools.append(modes,historyTools,done);
function configure(theme, nextScale) {
  for (const [name,value] of Object.entries(theme)) if (name.startsWith('--') && typeof value === 'string') host.style.setProperty(name,value);
  scale = nextScale || scale; layout();
}
function next() {
  if (queue.length) return Promise.resolve(queue.shift());
  if (closed) return Promise.resolve(null);
  return new Promise(resolve=>{waiter=resolve;});
}
function emit(value) { if (waiter) {const resolve=waiter;waiter=null;resolve(value);} else queue.push(value); }
function own(event) { return event.composedPath().includes(host); }
function stop(event) { event.preventDefault(); event.stopImmediatePropagation(); }
function editing() { return notes.some(note=>note.root.activeElement===note.input); }
function uiScale() { return Math.max(.125,Math.min(4,scale/(devicePixelRatio||1))); }
function point(event) { return {x:Math.round(Math.max(0,Math.min(innerWidth,event.clientX))+scrollX),y:Math.round(Math.max(0,Math.min(innerHeight,event.clientY))+scrollY)}; }
function box(a,b) { return {x:Math.min(a.x,b.x),y:Math.min(a.y,b.y),width:Math.abs(a.x-b.x),height:Math.abs(a.y-b.y)}; }
function bounds(points) { const xs=points.map(p=>p.x),ys=points.map(p=>p.y);return box({x:Math.min(...xs),y:Math.min(...ys)},{x:Math.max(...xs),y:Math.max(...ys)}); }
function rect(note) {
  if (note.kind==='element') {const r=note.node.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height};}
  const r=note.kind==='region'?note.region:note.stroke.bounds;
  return {x:r.x-scrollX,y:r.y-scrollY,width:r.width,height:r.height};
}
function remember() { history.push(notes.slice()); if(history.length>50) history.shift(); }
function payload(note, comment=note.input.value.trim()) {
  const elements=[];
  if(note.kind==='element') {
    const el=note.node, sensitive=el.closest('input,textarea,select,[contenteditable]');
    const id=el.id?'#'+CSS.escape(el.id):'';
    elements.push({tag:el.localName,selector:(el.localName+id+Array.from(el.classList).slice(0,3).map(c=>'.'+CSS.escape(c)).join('')).slice(0,300),text:sensitive?'':(el.innerText||'').replace(/\s+/g,' ').trim().slice(0,300)});
  }
  return JSON.stringify({id:note.id,comment,elements,regions:note.kind==='region'?[note.region]:[],strokes:note.kind==='draw'?[note.stroke]:[],viewport:{width:innerWidth,height:innerHeight,scroll_x:Math.round(scrollX),scroll_y:Math.round(scrollY)},page_title:document.title});
}
function synchronize(before, after) {
  for(const note of before) if(!after.includes(note) && note.submitted) emit(JSON.stringify({remove_id:note.id}));
  for(const note of after) if(!before.includes(note) && note.submitted) emit(note.submitted);
}
function replaceNotes(after) {
  const before=notes;notes=after;synchronize(before,after);
  for(const note of before) if(!after.includes(note)) {note.editor.remove();note.pin.remove();}
  if(!notes.some(n=>n.id===activeId)) activeId=null;
  layout();
}
function focus(note) {
  activeId=note.id; note.open=true; layout();
  note.input.focus({preventScroll:true});
  if(focusFrame!==null) cancelAnimationFrame(focusFrame);
  focusFrame=requestAnimationFrame(()=>{
    focusFrame=null;
    // Never refocus a text field which already owns the caret: doing so after
    // a pointer click can overwrite the insertion point the user just chose.
    if(!closed && activeId===note.id && notes.includes(note) && note.open && note.root.activeElement!==note.input && !editing()) note.input.focus({preventScroll:true});
  });
}
function setTool(value) {
  mode=value;drag=null;hovered=null;activeId=null;
  if(focusFrame!==null) {cancelAnimationFrame(focusFrame);focusFrame=null;}
  for(const note of notes) note.input.blur();
  layout();
}
function createNote(kind, target) {
  if(notes.length>=20) return;
  if(kind==='element') {const existing=notes.find(note=>note.kind===kind && note.node===target);if(existing) {focus(existing);return;}}
  remember();
  const number=++sequence;
  const note={id:crypto.randomUUID(),number,kind,open:true,submitted:null};
  if(kind==='element') note.node=target;
  else if(kind==='region') note.region=target;
  else note.stroke=target;
  const editor=document.createElement('div'); editor.dataset.zuseNote=note.id;editor.setAttribute('role','group');editor.setAttribute('aria-label',`Comment for annotation ${number}`);
  Object.assign(editor.style,{all:'initial',display:'block',position:'fixed',pointerEvents:'auto',boxSizing:'border-box'});
  const editorRoot=editor.attachShadow({mode:'closed'}), editorStyle=document.createElement('style');editorStyle.textContent=css;
  const panel=document.createElement('div');panel.className='panel';
  const header=document.createElement('div');header.className='heading';
  const badge=document.createElement('span');badge.className='number';badge.textContent=String(number);
  const anchor=document.createElement('span');anchor.className='anchor';anchor.textContent=kind==='element'?target.localName+(target.id?'#'+target.id:''):kind==='region'?'Region':'Drawing';
  const close=document.createElement('button');close.className='close';close.textContent='×';close.setAttribute('aria-label',`Hide comment for annotation ${number}`);
  header.append(badge,anchor,close);
  const input=document.createElement('textarea');input.rows=1;input.maxLength=4000;input.placeholder='Describe a change…';input.setAttribute('aria-label',`Comment for annotation ${number}`);
  const actions=document.createElement('div');actions.className='actions';
  const status=document.createElement('span');status.className='status';
  const add=document.createElement('button');add.className='add';add.textContent='Add to composer';add.setAttribute('aria-label',`Add annotation ${number} to composer`);
  actions.append(status,add);panel.append(header,input,actions);editorRoot.append(editorStyle,panel);
  const pin=document.createElement('button');pin.className='pin';pin.setAttribute('aria-label',`Edit annotation ${number}`);pin.title=`Annotation ${number}: ${anchor.textContent}`;
  const pinNumber=document.createElement('span');pinNumber.className='pin-number';pinNumber.textContent=String(number);pin.append(pinNumber);
  if(kind==='element') {const tag=document.createElement('span');tag.className='pin-tag';tag.textContent=target.localName;pin.append(tag);}
  Object.assign(note,{editor,root:editorRoot,input,add,status,pin});
  editorRoot.addEventListener('pointerdown',()=>{activeId=note.id;for(const other of notes) {other.editor.style.zIndex=other===note?'15':'10';other.pin.setAttribute('aria-pressed',String(other===note));}});
  input.addEventListener('input',()=>{input.style.height='0px';input.style.height=Math.min(108,Math.max(44,input.scrollHeight))+'px';layout();});
  input.addEventListener('keydown',event=>{
    if(event.key==='Enter' && !event.shiftKey && !event.isComposing) {event.preventDefault();commit(note);}
    event.stopPropagation();
  });
  add.addEventListener('click',()=>commit(note));
  close.addEventListener('click',()=>{note.open=false;note.input.blur();if(activeId===note.id)activeId=null;layout();});
  pin.addEventListener('click',()=>focus(note));
  notes.push(note);host.append(editor);pins.append(pin);focus(note);
}
function commit(note) {
  if(!note.input.value.trim() || note.add.disabled) return;
  note.submitted=payload(note);emit(note.submitted);layout();
}
function layout() {
  if(closed) return;
  const u=uiScale(), viewportWidth=innerWidth/u, viewportHeight=innerHeight/u, available=Math.max(120,viewportWidth-16);
  host.setAttribute('aria-label',`Browser annotations: ${mode}, ${notes.length} targets${editing()?', comment editor':''}`);
  tools.style.zoom=String(u);tools.style.maxWidth=available+'px';
  const contentTop=tools.getBoundingClientRect().bottom/u+8;
  for(const [id,button] of buttons) button.setAttribute('aria-pressed',String(id===mode));
  undo.disabled=!history.length;clear.disabled=!notes.length;
  const group=document.createElementNS(svg.namespaceURI,'g');group.setAttribute('transform',`translate(${-scrollX} ${-scrollY})`);
  function drawRect(r) {
    const node=document.createElementNS(svg.namespaceURI,'rect');for(const [key,value] of Object.entries(r))node.setAttribute(key,String(value));
    node.setAttribute('fill','var(--wash)');node.setAttribute('stroke','var(--accent)');node.setAttribute('stroke-width',String(2*u));group.append(node);
  }
  function drawPath(points) {
    const node=document.createElementNS(svg.namespaceURI,'path');node.setAttribute('d',points.map((p,i)=>`${i?'L':'M'}${p.x},${p.y}`).join(' '));
    node.setAttribute('fill','none');node.setAttribute('stroke','var(--accent)');node.setAttribute('stroke-width',String(3*u));node.setAttribute('stroke-linecap','round');node.setAttribute('stroke-linejoin','round');group.append(node);
  }
  for(const note of notes) {
    const r=rect(note);
    if(note.kind==='draw')drawPath(note.stroke.points);else drawRect({x:r.x+scrollX,y:r.y+scrollY,width:r.width,height:r.height});
    if(!note.editor.isConnected)host.append(note.editor);if(!note.pin.isConnected)pins.append(note.pin);
    note.pin.style.zoom=String(u);note.pin.style.left=Math.max(2,Math.min(r.x/u-10,viewportWidth-note.pin.getBoundingClientRect().width/u-2))+'px';note.pin.style.top=Math.max(2,Math.min(r.y/u-10,viewportHeight-24))+'px';note.pin.setAttribute('aria-pressed',String(activeId===note.id));
    note.editor.hidden=!note.open;note.editor.style.display=note.open?'block':'none';note.editor.style.zoom=String(u);note.editor.style.width=Math.min(304,available)+'px';note.editor.style.maxHeight=Math.max(0,viewportHeight-contentTop-8)+'px';note.editor.style.zIndex=activeId===note.id?'15':'10';
    const dirty=note.submitted===null || JSON.parse(note.submitted).comment!==note.input.value.trim();
    note.add.disabled=!note.input.value.trim()||!dirty;note.add.textContent=note.submitted?'Update in composer':'Add to composer';note.status.textContent=note.submitted?(dirty?'Edited':'Added'):'↵ to add';
    // Anchor directly below this mark. Only flip above when the viewport has
    // no room; resizing or focusing another note never rebuilds its textarea.
    const width=note.editor.getBoundingClientRect().width/u, height=note.editor.getBoundingClientRect().height/u;
    const left=Math.max(8,Math.min(r.x/u,viewportWidth-width-8));
    let top=(r.y+r.height)/u+8;
    if(top+height>viewportHeight-8)top=r.y/u-height-8;
    top=Math.max(contentTop,Math.min(top,viewportHeight-height-8));
    note.editor.style.left=left+'px';note.editor.style.top=top+'px';
  }
  if(drag?.kind==='region')drawRect(box(drag.start,drag.end));
  if(drag?.kind==='draw')drawPath(drag.points);
  svg.replaceChildren(group);
  const valid=mode==='select' && hovered?.isConnected && !notes.some(note=>note.node===hovered);
  hover.hidden=!valid;
  hoverLabel.hidden=!valid;
  if(valid) {
    const r=hovered.getBoundingClientRect();Object.assign(hover.style,{left:r.x+'px',top:r.y+'px',width:r.width+'px',height:r.height+'px',borderWidth:String(u)+'px'});
    hoverLabel.textContent=hovered.localName;hoverLabel.style.zoom=String(u);
    hoverLabel.style.left=Math.max(2,Math.min(r.x/u,viewportWidth-hoverLabel.getBoundingClientRect().width/u-2))+'px';hoverLabel.style.top=Math.max(2,r.y/u-22)+'px';
  }
}
function hit(p,r) {return p.x>=r.x && p.y>=r.y && p.x<=r.x+r.width && p.y<=r.y+r.height;}
function down(event) {
  if(own(event)||event.button!==0)return;stop(event);
  if(mode==='select')return;
  const p=point(event);
  if(mode==='erase') {
    const note=notes.slice().reverse().find(note=>{const r=rect(note);return hit(p,{x:r.x+scrollX-4,y:r.y+scrollY-4,width:r.width+8,height:r.height+8});});
    if(note){remember();replaceNotes(notes.filter(n=>n!==note));}return;
  }
  drag={kind:mode,start:p,end:p,points:[p]};layout();
}
function move(event) {
  if(drag) {stop(event);const p=point(event);drag.end=p;const last=drag.points[drag.points.length-1];if(drag.kind==='draw'&&drag.points.length<2048&&Math.hypot(p.x-last.x,p.y-last.y)>=1)drag.points.push(p);layout();return;}
  const target=mode==='select'&&!own(event)&&event.target instanceof Element?event.target:null;
  if(target!==hovered) {hovered=target;layout();}
}
function up(event) {
  if(own(event)&&!drag)return;stop(event);
  if(!drag)return;
  const active=drag;drag=null;const p=point(event);
  if(active.kind==='region') {const r=box(active.start,p);if(r.width>=4&&r.height>=4)createNote('region',r);}
  else {if(active.points.length<2048)active.points.push(p);const r=bounds(active.points);if(r.width>=4||r.height>=4)createNote('draw',{points:active.points,bounds:r});}
  layout();
}
function click(event) {if(own(event))return;stop(event);if(mode==='select'&&event.target instanceof Element)createNote('element',event.target);}
function block(event) {if(!own(event))stop(event);}
function cancelDrag(){drag=null;layout();}
function key(event) {
  if(!editing()&&!event.isComposing) {
    if((event.ctrlKey||event.metaKey)&&event.key.toLowerCase()==='z'){stop(event);undo.click();return;}
    if(event.key==='Delete'){stop(event);clear.click();return;}
    if(!event.ctrlKey&&!event.metaKey&&!event.altKey){const tool={'1':'select','2':'region','3':'draw','4':'erase'}[event.key];if(tool){stop(event);setTool(tool);return;}}
  }
  if(event.key==='Escape'&&!event.isComposing){stop(event);close();}
}
function close() {
  if(closed)return;closed=true;if(focusFrame!==null)cancelAnimationFrame(focusFrame);
  document.removeEventListener('pointerdown',down,true);document.removeEventListener('pointermove',move,true);document.removeEventListener('pointerup',up,true);document.removeEventListener('pointercancel',cancelDrag,true);document.removeEventListener('click',click,true);document.removeEventListener('keydown',key,true);
  for(const name of ['mousedown','mouseup'])document.removeEventListener(name,block,true);
  window.removeEventListener('scroll',layout,true);window.removeEventListener('resize',layout);host.remove();emit(null);
}
undo.addEventListener('click',()=>{const previous=history.pop();if(previous)replaceNotes(previous);});
clear.addEventListener('click',()=>{if(notes.length){remember();replaceNotes([]);}drag=null;layout();});
done.addEventListener('click',close);
globalThis.zuseAnnotationSession={generation,configure,next};
globalThis.zuseSetAnnotationTheme=theme=>configure(theme,scale);
globalThis.zuseCancelSelection=close;
document.addEventListener('pointerdown',down,true);document.addEventListener('pointermove',move,true);document.addEventListener('pointerup',up,true);document.addEventListener('pointercancel',cancelDrag,true);document.addEventListener('click',click,true);document.addEventListener('keydown',key,true);
for(const name of ['mousedown','mouseup'])document.addEventListener(name,block,true);
window.addEventListener('scroll',layout,true);window.addEventListener('resize',layout);
configure(palette,displayScale);
return await next();
