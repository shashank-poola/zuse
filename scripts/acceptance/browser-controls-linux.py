#!/usr/bin/env python3
"""Exercise real WebKitGTK controls, using a built zuse-webkit helper.

Run from the repository root with a desktop display (or xvfb-run):
python3 scripts/acceptance/browser-controls-linux.py /path/to/zuse-webkit
Requires xwininfo to verify the developer-tools window actually appears.
The helper uses ephemeral storage and a local fixture; no provider is involved.
Set ZUSE_BROWSER_TEST_SCALE=2 to exercise a HiDPI viewport, and optionally
ZUSE_BROWSER_TEST_SCREENSHOT=/path/to/editor.png to save the in-page editor.
"""
import http.server
import json
import os
from pathlib import Path
import queue
import struct
import subprocess
import sys
import tempfile
import threading
import time


class Site(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        body = (
            b'<style>button,input{display:block;margin:30px;width:200px;height:40px}</style>'
            b'<button id="pick" onclick="window.clicked=1">Pick this button</button>'
            b'<input id="secret" type="password" value="private-value">'
            b'<button id="other" style="margin-top:140px">Another target</button>'
        )
        self.send_response(200)
        self.send_header("Content-Type", "text/html")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *_):
        pass


def main(helper):
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Site)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}/"
    page_scale = float(os.environ.get("ZUSE_BROWSER_TEST_SCALE", "1"))
    # Fixed GTK scaling makes synthetic coordinates deterministic on HiDPI hosts.
    env = dict(os.environ, GDK_SCALE="1", GDK_DPI_SCALE="1")
    with tempfile.TemporaryFile() as log:
        proc = subprocess.Popen([helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                stderr=log, env=env)
        packets = queue.Queue()

        def read():
            try:
                while True:
                    header = proc.stdout.read(9)
                    if not header:
                        packets.put((b"!", b"Browser helper exited"))
                        return
                    kind, _, length = struct.unpack("<cII", header)
                    data = proc.stdout.read(length)
                    if kind not in (b"F", b"I", b"S"):
                        packets.put((kind, data))
            except Exception as error:
                packets.put((b"!", str(error).encode()))

        threading.Thread(target=read, daemon=True).start()

        def send(command, **values):
            data = json.dumps(dict(id=1, cmd=command, **values)).encode()
            proc.stdin.write(struct.pack("<I", len(data)) + data)
            proc.stdin.flush()

        deferred = {}
        def wait(kind):
            if deferred.get(kind): return deferred[kind].pop(0)
            deadline = time.monotonic() + 15
            while time.monotonic() < deadline:
                received, data = packets.get(timeout=max(.01, deadline - time.monotonic()))
                if received in (b"X", b"!"):
                    raise AssertionError(data.decode())
                if received == kind:
                    return data
                deferred.setdefault(received, []).append(data)
            raise AssertionError(f"Missing browser response {kind!r}")

        def js(script):
            send("eval", script=script)
            return json.loads(wait(b"J"))

        def until(condition):
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                if condition():
                    return
                time.sleep(.05)
            raise AssertionError("Browser condition did not become true")

        palette = {"--fg":"#242424","--muted":"#777777","--surface":"#ffffff",
                   "--border":"#dddddd","--hover":"#eeeeee","--accent":"#6750a4",
                   "--wash":"#6750a420","--hover-wash":"#6750a410","--primary":"#242424",
                   "--on-primary":"#ffffff","--shadow":"#00000020","--font":"system-ui"}
        source = Path("crates/ui/src/browser/select_element.js").read_text().replace(
            "attachShadow({mode:'closed'})", "attachShadow({mode:'open'})")

        def pull(generation):
            script = f"const zuseAnnotationGeneration = {generation};\nconst zuseAnnotationTheme = " + json.dumps(palette) + ";\n" + source
            send("select-element", page=url, generation=generation, script=script)

        def selection(generation):
            while True:
                result = json.loads(wait(b"A"))
                if result["generation"] == generation:
                    return None if result["details"] is None else json.loads(result["details"])

        def key(name, text="", mods=0):
            send("key_down", key=name, text=text, mods=mods)
            send("key_up", key=name, text=text, mods=mods)
            time.sleep(.025)

        def type_keys(text):
            for character in text:
                key("space" if character == " " else character, character)
                time.sleep(.04)

        def pointer(command, x, y):
            ratio = js("devicePixelRatio") / page_scale
            send(command, x=x*ratio, y=y*ratio, button=1, mods=256 if command == "move" else 0)
            time.sleep(.04)

        def click(x, y):
            pointer("down", x, y); pointer("up", x, y)

        def click_element(selector):
            x,y = js(f"(()=>{{let r=document.querySelector({json.dumps(selector)}).getBoundingClientRect();return [r.x+r.width/2,r.y+r.height/2]}})()")
            click(x,y)

        def state():
            return js("document.querySelector('[data-zuse-annotation]').getAttribute('aria-label')")

        def note_rect(index):
            return js(f"(()=>{{let r=document.querySelectorAll('[data-zuse-note]')[{index}].getBoundingClientRect();return [r.x,r.y,r.width,r.height]}})()")

        def focus_note(index, prefix=None):
            time.sleep(.15)
            x,y=js(f"""(()=>{{let e=document.querySelectorAll('[data-zuse-note]')[{index}].shadowRoot.querySelector('textarea');
                let r=e.getBoundingClientRect(),c=document.createElement('canvas').getContext('2d'),style=getComputedStyle(e);
                c.font=style.fontSize+' '+style.fontFamily;let width=c.measureText({json.dumps(prefix or '')}).width;
                let scale={page_scale}/devicePixelRatio;return [r.left+(2+width)*scale,r.top+10*scale]}})()""")
            click(x,y)
            until(lambda: js(f"document.activeElement === document.querySelectorAll('[data-zuse-note]')[{index}]"))

        def begin(generation, tool=1):
            pull(generation)
            until(lambda: js("!!document.querySelector('[data-zuse-annotation]')"))
            key(str(tool),str(tool))
            mode={1:'select',2:'region',3:'draw',4:'erase'}[tool]
            until(lambda: f"{mode}, 0 targets" in state())

        def drag(points):
            pointer("down",*points[0])
            for p in points[1:]: pointer("move",*p)
            pointer("up",*points[-1])
            until(lambda: js("!!document.activeElement?.hasAttribute('data-zuse-note')"))

        def inspector_visible():
            for _ in range(10):
                result=subprocess.run(["xwininfo","-root","-tree"],text=True,capture_output=True)
                if result.returncode==0: return "Zuse Developer Tools" in result.stdout
                time.sleep(.02)
            raise AssertionError(result.stderr)

        try:
            wait(b"R");send("create")
            send("resize",width=800*page_scale,height=1000*page_scale,scale=page_scale)
            send("load",url=url)
            until(lambda: js("document.querySelector('#pick') !== null"))
            begin(1)
            toolbar=js("""(()=>{let t=document.querySelector('[data-zuse-annotation]').shadowRoot.querySelector('.tools');
                let r=t.getBoundingClientRect();return {left:r.left,right:r.right,rows:[...t.querySelectorAll('button')].map(b=>b.getBoundingClientRect().top)}})()""")
            assert toolbar['left']>=0 and toolbar['right']<=js('innerWidth'),toolbar
            assert len(set(toolbar['rows']))==1,toolbar
            click_element('#pick')
            until(lambda: js("!!document.activeElement?.hasAttribute('data-zuse-note')"))
            assert js("document.querySelector('[data-zuse-annotation]').shadowRoot.querySelector('.pin-tag').textContent")=='button'
            assert 'system-ui' in js("getComputedStyle(document.querySelector('[data-zuse-note]').shadowRoot.querySelector('.anchor')).fontFamily")
            type_keys('hello world')
            # Pointer placement inside existing text must move the real caret.
            focus_note(0,'hello')
            type_keys('x')
            custom=dict(palette, **{"--surface":"#17191b","--fg":"#eeeeee","--primary":"#eeeeee","--on-primary":"#17191b","--accent":"#ff8800"})
            send('annotation-theme',theme=json.dumps(custom))
            until(lambda: js("document.querySelector('[data-zuse-annotation]').style.getPropertyValue('--surface') === '#17191b'"))
            # The editor belongs directly below the first mark.
            r=note_rect(0)
            target=js("(()=>{let r=document.querySelector('#pick').getBoundingClientRect();return [r.left,r.bottom]})()")
            assert r[1]>=target[1] and r[1]-target[1]<20, (r,target)
            # A second target gets an independent editor, preserving the first.
            click_element('#other')
            until(lambda: js("document.querySelectorAll('[data-zuse-note]').length === 2"))
            send('commit',text='second draft')
            # Click the first numbered pin and submit that note, not the second.
            pin=js("(()=>{let r=document.querySelector('#pick').getBoundingClientRect();return [r.x,r.y]})()")
            u=page_scale/js('devicePixelRatio')
            click(pin[0]+u,pin[1]+u)
            until(lambda: js("document.activeElement === document.querySelectorAll('[data-zuse-note]')[0]"))
            screenshot=os.environ.get('ZUSE_BROWSER_TEST_SCREENSHOT')
            if screenshot:
                send('screenshot');Path(screenshot).write_bytes(wait(b'P'))
            key('Return')
            first=selection(1)
            assert first['comment']=='hellox world',first
            assert first['elements'][0]['selector']=='button#pick',first
            assert js('window.clicked || 0')==0
            pull(1) # The native consumer asks for another result; editors persist.
            focus_note(1);key('End');type_keys('!');key('Return')
            second=selection(1)
            assert second['comment']=='second draft!',second
            assert second['id']!=first['id']
            assert js("document.querySelectorAll('[data-zuse-note]').length")==2
            print('PASS caret clicks, numbered pins, anchored independent editors and IME typing',flush=True)
            # Updating one pin replaces its own staged note using the same ID.
            pull(1);focus_note(0);key('End');type_keys(' updated');key('Return')
            updated=selection(1)
            assert updated['id']==first['id'] and updated['comment']=='hellox world updated',updated
            print('PASS updating a note preserves its annotation identity',flush=True)
            begin(2);click_element('#secret');until(lambda: js("!!document.activeElement?.hasAttribute('data-zuse-note')"))
            send('commit',text='private field');key('Return')
            private=selection(2)
            assert private['elements'][0]['text']==''
            begin(3,2);drag([(25,140),(70,160),(110,180)]);type_keys('fix region');key('Return')
            region=selection(3)
            assert not region['elements'] and len(region['regions'])==1 and region['regions'][0]['width']==85,region
            begin(4,3);drag([(25,140),(40,155),(55,140),(70,175)]);type_keys('fix drawing');key('Return')
            stroke=selection(4)
            assert len(stroke['strokes'])==1 and len(stroke['strokes'][0]['points'])>=3,stroke
            print('PASS region and drawing comments are separate annotations',flush=True)
            # Erasing a submitted note also removes its composer reference;
            # undo restores exactly the committed version.
            pull(4)
            r=note_rect(0);u=page_scale/js('devicePixelRatio')
            click(r[0]+r[2]-21*u,r[1]+21*u) # Collapse only this editor.
            key('4','4');until(lambda:'erase, 1 targets' in state())
            click(40,155)
            removed=selection(4);assert removed['remove_id']==stroke['id'],removed
            pull(4);key('z','z',mods=4)
            restored=selection(4);assert restored['id']==stroke['id'] and restored['comment']=='fix drawing',restored
            pull(4);key('Delete');removed=selection(4);assert removed['remove_id']==stroke['id']
            pull(4);key('z','z',mods=4);assert selection(4)['id']==stroke['id']
            pull(4);send('cancel-selection');assert selection(4) is None
            print('PASS erase, clear and undo stay synchronized with staged notes',flush=True)
            send('screenshot');assert wait(b'P').startswith(b'\x89PNG\r\n\x1a\n')
            js("document.cookie='control_test=yes; path=/'")
            assert 'control_test=yes' in js('document.cookie')
            send('clear-data',cookies=True);assert wait(b'D')==b'cookies';assert 'control_test' not in js('document.cookie')
            send('clear-data',cookies=False);assert wait(b'D')==b'cache'
            send('appearance',dark=True);until(lambda:js("matchMedia('(prefers-color-scheme: dark)').matches"))
            send('appearance',dark=False);until(lambda:js("!matchMedia('(prefers-color-scheme: dark)').matches"))
            send('devtools');until(inspector_visible);send('devtools');until(lambda:not inspector_visible())
            assert js('1+1')==2
            print('PASS screenshot, cookies/cache, website appearance and inspector',flush=True)
        except Exception:
            log.seek(0);print(log.read().decode(errors='replace'),file=sys.stderr);raise
        finally:
            proc.stdin.close()
            try: proc.wait(timeout=5)
            except subprocess.TimeoutExpired: proc.kill();proc.wait()
            server.shutdown();server.server_close()


if __name__ == '__main__':
    if len(sys.argv)!=2: sys.exit(__doc__)
    main(str(Path(sys.argv[1]).resolve()))
