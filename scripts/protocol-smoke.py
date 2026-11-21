"""Exercise the Obsidian extension over protocol 1 with owned files and a recording host."""
import argparse
from contextlib import contextmanager
from datetime import date
import hashlib
import json
import os
from pathlib import Path
import queue
import subprocess
import sys
import tempfile
import threading
from urllib.parse import parse_qs, urlsplit
import uuid
import zipfile

ROOT = Path(__file__).resolve().parents[1]
MANIFEST = json.loads((ROOT / 'extension.json').read_bytes())


def put(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(data if isinstance(data, bytes) else data.encode())


def data(path, value):
    put(path, json.dumps(value, ensure_ascii=False, indent=2))


class Host:
    def __init__(self, process, root, platform, package):
        self.process = process
        self.inbox = queue.Queue()
        self.effects = []
        self.selection = 'selected fixture words'
        self.clipboard = ''
        self.confirm = True
        self.after_confirm = None
        self.tray = None
        self.leases = {}
        def read():
            try:
                for line in self.process.stdout:
                    self.inbox.put(json.loads(line))
            finally:
                self.inbox.put(None)
        self.reader = threading.Thread(target=read, daemon=True)
        self.reader.start()
        hello = self.inbox.get(timeout=12)
        assert [hello[key] for key in ('type','id','version','protocol')] == ['hello',MANIFEST['id'],MANIFEST['version'],1]
        self.rpc('initialize',dict(protocol=1,extensionId=MANIFEST['id'],platform=platform,packageDir=str(package),
            dataDir=str(root/'tezbar-data'),tempDir=str(root/'tezbar-temp'),grantedCapabilities=MANIFEST['capabilities'],migrationPaths={}))

    def send(self, frame):
        self.process.stdin.write((json.dumps(frame,ensure_ascii=False) + '\n').encode())
        self.process.stdin.flush()

    def effect(self, method, params):
        self.effects.append((method,params))
        if method == 'host.clipboard.read':
            return self.clipboard
        if method == 'host.selection.read':
            return self.selection
        if method == 'host.dialog.confirm':
            if self.after_confirm:
                self.after_confirm()
            return {'response':int(self.confirm)}
        if method == 'host.background.retain':
            assert params['durable'] is True
            self.leases[params['key']] = params.get('data')
        elif method == 'host.background.release':
            self.leases.pop(params['key'],None)
        elif method == 'host.tray.set':
            self.tray = params
        elif method == 'host.tray.remove':
            self.tray = None
        else:
            assert method in ['host.window.visibility','host.shell.open','host.shell.reveal','host.settings.open',
                              'host.clipboard.write','host.clipboard.pasteText'],(method,params)
        return True

    def rpc(self, method, params, *, error=False):
        ident=f'h:{uuid.uuid4()}'
        self.send(dict(type='request',id=ident,method=method,params=params))
        while True:
            frame=self.inbox.get(timeout=25)
            assert frame is not None,'Extension ended before its response'
            if frame['type']=='request':
                result=self.effect(frame['method'],frame['params'])
                self.send(dict(type='response',id=frame['id'],result=result))
            elif frame.get('id') == ident:
                assert ('error' in frame)==error,frame
                return frame['error']['message'] if error else frame['result']
            else:
                assert frame['type'] in ['event','cancel'],frame

    def open(self, command, sid=None, prefs=None, args=None):
        return self.rpc('session.open',dict(sessionId=sid or command,commandName=command,preferences=prefs or {},argumentValues=args or {}))

    def call(self, sid, operation, *, error=False, **payload):
        return self.rpc('session.call',dict(sessionId=sid,operation=operation,payload=payload),error=error)

    def act(self, sid, action, *, error=False, **kwargs):
        return self.call(sid,'extension:invoke-action',error=error,actionId=action,**kwargs)


@contextmanager
def session(binary, root, platform, package=ROOT, use_fixture=True):
    ext = [str(binary)] + (['--fixtures',str(root)] if use_fixture else [])
    with (root/'stderr.log').open('wb+') as errors:
        p = subprocess.Popen(ext,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=errors)
        try:
            yield Host(p,root,platform,package)
        finally:
            p.stdin.close()
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
                p.wait(timeout=5)
            if p.returncode != 0:
                errors.seek(0)
                print(errors.read().decode(errors='replace'),file=sys.stderr)
            assert p.returncode==0


def nodes(root):
    yield root
    for child in root.get('children',[]):
        yield from nodes(child)


def rows(view):
    assert view['mode']=='view',view
    return [row['props'] for row in nodes(view['root']) if row['type'] in ['List.Item','Grid.Item']]


def action(view, label, *, row=None, path=None):
    ids = None
    if row is not None or path is not None:
        ids=next(r['actionIds'] for r in rows(view) if (row is None or row == r['title']) and (path is None or path in r['id']))
    for item in view['actions']:
        if item['title']==label and (ids is None or item['id'] in ids):
            return item['id']
    raise AssertionError((label,row,path,[(a['title'],a['id']) for a in view['actions']]))


def field(view,id):
    assert view['root']['type']=='Form',view['root']['type']
    return next(x['props'] for x in nodes(view['root']) if x['props'].get('id')==id)


def last_target(host):
    return next(params['target'] for method,params in reversed(host.effects) if method=='host.shell.open')


def setup(root):
    put(root/'.tezbar-obsidian-fixture','tezbar-obsidian-fixture-v1\n')
    vault=root/'left/Notes'; duplicate=root/'right/Notes'
    duplicate.mkdir(parents=True)
    config=vault/'.fixture-settings'
    data(config/'bookmarks.json',{'items':[{'type':'group','title':'Nested','items':[{'type':'file','path':'Projects/Alpha.md','title':'Bookmark A'}]}],'preserve':1})
    data(config/'app.json',{'userIgnoreFilters':['Ignored']})
    data(config/'core-plugins.json',{'daily-notes':True,'workspaces':True})
    data(config/'community-plugins.json',['obsidian-advanced-uri'])
    data(config/'workspaces.json',{'workspaces':{'Research & Plans':{'main':{}}},'active':'Research & Plans'})
    data(config/'daily-notes.json',{'folder':'Daily','format':'YYYY-MM-DD','template':'Templates/Daily Template'})
    put(vault/'Projects/Alpha.md','---\ntags: [project/core]\n---\n# Alpha\nA literal needle #inline\n```rust\nlet fixture = 1;\n```\n')
    put(vault/'Mirror/Alpha.md','Separate note with same title\n')
    put(vault/'Ignored/Hidden.md','#project literal needle private ignore test\n')
    put(vault/'Secret/Excluded.md','blocked by preference\n')
    put(vault/'Trash Me.md','Owned trash fixture\n')
    put(vault/'Tasks.md','# Todo\nExisting\n# After\nintact\n')
    put(vault/'Shopping.md','Existing shopping data\n')
    put(vault/'Templates/Daily Template.md','# Log\nTemplate {{date}}\n# After\nKeep\n')
    put(vault/'Attachments/Picture.png',b'owned synthetic media bytes')
    put(vault/'Attachments/Paper.pdf',b'owned synthetic PDF bytes')
    put(duplicate/'Mirror/Alpha.md','Never change duplicate vault\n')
    for platform in ('Windows','macOS'):
        data(root/platform/'obsidian.json',{'vaults':{'left123':{'path':str(vault),'ts':200,'open':True},'right456':{'path':str(duplicate),'ts':100,'open':False}}})
    return vault,duplicate


def fixture_suite(binary,root,checks):
    vault,duplicate=setup(root)
    prefer={'vaultPath':str(vault),'configFileName':'.fixture-settings','excludedFolders':'Secret'}
    with session(binary,root,'obsidian-fixture-windows') as host:
        vault_view=host.open('openVaultCommand',prefs={'configFileName':'.fixture-settings'})
        assert [r['title'] for r in rows(vault_view)]==['Notes','Notes']
        host.act('openVaultCommand',action(vault_view,'Open Vault',path='vault:1'))
        assert parse_qs(urlsplit(last_target(host)).query)=={'vault':['right456']}
        assert duplicate.joinpath('Mirror/Alpha.md').read_text()=='Never change duplicate vault\n'
        checks.append('openVaultCommand discovers two registered duplicate names and opens by exact vault ID')

        query_prefs={**prefer,'searchContent':True,'removeYAML':True,'showMetadata':True}
        view=host.open('searchNoteCommand',prefs=query_prefs,args={'searchArgument':'literal needle','tagArgument':'project'})
        matched=next(r for r in rows(view) if 'Projects/Alpha.md' in r['id'])
        assert matched['accessories'][0]=={'text':'L5:3'},matched
        assert not any('Ignored/' in r['id'] or 'Secret/' in r['id'] for r in rows(view))
        copy=action(view,'Copy Note Content',path='Projects/')
        assert next(x['shortcut']['modifiers'] for x in view['actions'] if x['id']==copy)==['ctrl']
        previous_id=copy
        view=host.act('searchNoteCommand',copy)
        assert host.effects[-1][0]=='host.clipboard.write' and host.effects[-1][1]['text'].startswith('# Alpha')
        assert 'Stale' in host.act('searchNoteCommand',previous_id,error=True)
        hit=action(view,'Open Match in Obsidian',path='Projects/')
        host.act('searchNoteCommand',hit)
        assert parse_qs(urlsplit(last_target(host)).query)=={'vault':['left123'],'filepath':['Projects/Alpha.md'],'line':['5'],'column':['3']}

        literal=(
            '  Literal {date}, {clipboard}, {selection}\r\n'
            r'Escaped \{date\}, \{clipboard\}, \{selection\}' '\r\n'
            '`{date}` {{clipboard}}\r\n```text\r\n{selection}\r\n```\r\n\r\n'
        )
        mirror=vault/'Mirror/Alpha.md'
        project=vault/'Projects/Alpha.md'
        mirror_before=mirror.read_bytes()
        project_before=project.read_bytes()
        append_template='Capture {date}: {clipboard} / {selection}'
        view=host.open('searchNoteCommand','duplicates',{**prefer,'appendTemplate':append_template,
            'appendSelectedTemplate':'Selected {date}: {content}'},args={'searchArgument':'Alpha'})
        assert len([r for r in rows(view) if r['title']=='Alpha'])==2
        host.clipboard='clipboard fixture sentinel'
        before=len(host.effects)
        draft=host.act('duplicates',action(view,'Append to Note',path='Mirror/Alpha.md'))
        assert field(draft,'content')['defaultValue']==append_template
        assert draft['actions'][0]['title']=='Save'
        oversized='x'*(512*1024+1)
        draft=host.act('duplicates',action(draft,'Save'),formValues={'content':oversized})
        assert field(draft,'content')['defaultValue']==oversized
        assert mirror.read_bytes()==mirror_before
        view=host.act('duplicates',action(draft,'Save'),formValues={'content':literal})
        assert mirror.read_bytes()==mirror_before+b'\n'+literal.encode()
        assert project.read_bytes()==project_before
        assert view['effects'][0]['title']=='Added text to note'

        put(project,project_before+literal.encode())
        edit=host.act('duplicates',action(view,'Edit Note',path='Projects/Alpha.md'))
        assert field(edit,'content')['defaultValue'].encode()==project_before+literal.encode()
        edited=literal+'Final edit without a newline'
        original=project.read_bytes()
        host.after_confirm=lambda:put(project,original+b'External owned update\n')
        blocked=host.act('duplicates',action(edit,'Save'),formValues={'content':edited})
        host.after_confirm=None
        assert field(blocked,'content')['defaultValue']==edited
        assert project.read_bytes()==original+b'External owned update\n'
        assert 'effects' not in blocked
        view=host.act('duplicates',action(blocked,'Back'))
        edit=host.act('duplicates',action(view,'Edit Note',path='Projects/Alpha.md'))
        view=host.act('duplicates',action(edit,'Save'),formValues={'content':edited})
        assert project.read_bytes()==edited.encode()
        assert view['effects'][0]['title']=='Edited note'
        assert [method for method,_ in host.effects[before:]
                if method.startswith(('host.clipboard.','host.selection.'))]==[]

        before=len(host.effects)
        draft=host.act('duplicates',action(view,'Append to Note',path='Mirror/Alpha.md'))
        view=host.act('duplicates',action(draft,'Save with Template Expansion'),
            formValues={'content':append_template})
        assert mirror.read_bytes().endswith(
            f'\nCapture {date.today()}: {host.clipboard} / {host.selection}'.encode())
        assert [method for method,_ in host.effects[before:]]==[
            'host.clipboard.read','host.selection.read']
        before=len(host.effects)
        view=host.act('duplicates',action(view,'Append Selected Text to Note',path='Mirror/Alpha.md'))
        assert mirror.read_bytes().endswith(f'\nSelected {date.today()}: {host.selection}'.encode())
        assert [method for method,_ in host.effects[before:]]==['host.selection.read']
        assert view['effects'][0]['title']=='Added selected text to note'

        expected={f'Pagination Fixture {i:02d}' for i in range(60)}
        for title in sorted(expected):
            put(vault/f'Pages/{title}.md','Owned pagination fixture\n')
        paged=host.open('searchNoteCommand','paged',{**prefer,'showDetail':False},
            args={'searchArgument':'Pagination Fixture'})
        assert paged['root']['props']['__hasServerSearch'] is True
        assert paged['root']['props']['pagination']=={'hasMore':True}
        first_page={r['title'] for r in rows(paged)}
        assert len(first_page)==45 and first_page < expected
        paged=host.call('paged','extension:load-more')
        assert {r['title'] for r in rows(paged)}==expected
        assert paged['root']['props']['pagination']=={'hasMore':False}
        hidden=min(expected-first_page)
        paged=host.call('paged','extension:search-text-changed',searchText='No fixture matches this')
        assert not rows(paged) and paged['root']['props']['pagination']=={'hasMore':False}
        paged=host.call('paged','extension:search-text-changed',searchText=hidden)
        assert [r['title'] for r in rows(paged)]==[hidden]
        checks.append('searchNoteCommand covers server paging and late matches, byte-exact raw drafts, explicit templates, success feedback and edit conflicts')

        starred=host.open('starredNotesCommand',prefs=prefer)
        assert [r['title'] for r in rows(starred)]==['Alpha']
        starred=host.act('starredNotesCommand',action(starred,'Unbookmark Note',path='Projects/Alpha.md'))
        cfg=json.loads((vault/'.fixture-settings/bookmarks.json').read_bytes())
        assert cfg=={'items':[{'type':'group','title':'Nested','items':[]}],'preserve':1},cfg
        full=host.open('searchNoteCommand','bookmark-again',prefer,args={'searchArgument':'Alpha'})
        host.act('bookmark-again',action(full,'Bookmark Note',path='Projects/Alpha.md'))
        assert any(i['path']=='Projects/Alpha.md' for i in json.loads((vault/'.fixture-settings/bookmarks.json').read_bytes())['items'] if i['type']=='file')
        checks.append('starredNotesCommand reads nested bookmarks and updates exactly one note while preserving JSON metadata')

        workspace=host.open('openWorkspaceCommand',prefs=prefer)
        assert rows(workspace)[0]['subtitle']=='Active'
        host.act('openWorkspaceCommand',action(workspace,'Open Workspace',row='Research & Plans'))
        assert parse_qs(urlsplit(last_target(host)).query)=={'vault':['left123'],'workspace':['Research & Plans']}
        checks.append('openWorkspaceCommand reads selected config and percent-encodes the exact workspace')

        today=vault/'Daily'/f'{date.today()}.md'
        host.open('dailyNoteCommand',prefs=prefer)
        assert f'Template {date.today()}' in today.read_text() and parse_qs(urlsplit(last_target(host)).query)['file'][0].endswith(f'{date.today()}.md')
        before=len(host.effects)
        host.open('dailyNoteAppendCommand',prefs={**prefer,'heading':'Log','prepend':True,'silent':True,'appendTemplate':'Daily {content}'},args={'text':'capture'})
        assert '# Log\nDaily capture\nTemplate' in today.read_text()
        assert all(method!='host.shell.open' for method,_ in host.effects[before:])
        data(vault/'.fixture-settings/daily-notes.json',
            {'folder':'Daily New','format':'YYYY-MM-DD','template':'Templates/Daily Template'})
        new_daily=vault/'Daily New'/today.name
        assert not new_daily.exists()
        blocked=host.open('dailyNoteAppendCommand','daily-no-heading',
            {**prefer,'heading':'Missing','silent':True},args={'text':'Retained daily draft'})
        assert field(blocked,'text')['defaultValue']=='Retained daily draft'
        assert not new_daily.exists()
        host.open('dailyNoteAppendCommand','daily-from-seed',
            {**prefer,'heading':'## Log','silent':True},args={'text':'Created under Log'})
        assert f'# Log\nTemplate {date.today()}\nCreated under Log\n# After\nKeep\n'==new_daily.read_text()
        checks.append('dailyNoteCommand and dailyNoteAppendCommand resolve the template, insert under its heading on creation and retain drafts for missing headings')

        form=host.open('createNoteCommand',prefs={**prefer,'fillFormWithDefaults':True,'prefNoteName':'Prefilled','prefNoteContent':'Prefill draft','prefTag':'owned','tags':'owned, other','folderActions':'Special','openOnCreate':False})
        assert field(form,'name')['defaultValue']=='Prefilled'
        assert field(form,'tags')['defaultValue']==['owned']
        assert [n['props']['value'] for n in nodes(form['root']) if n['type']=='Form.TagPicker.Item']==['owned','other']
        host.act('createNoteCommand',action(form,'Create in Special'),formValues={'name':'New Note','content':'New draft','tags':['owned']})
        assert 'New draft' in (vault/'Special/New Note.md').read_text()
        form=host.open('createNoteCommand','conflict',{**prefer,'openOnCreate':False})
        form=host.act('conflict',action(form,'Create'),formValues={'name':'New Note','path':'Special','content':'Preserved conflict draft'})
        assert field(form,'content')['defaultValue']=='Preserved conflict draft'
        assert 'Preserved conflict draft' not in (vault/'Special/New Note.md').read_text()
        checks.append('createNoteCommand honors prefill/folder actions, protects existing notes and retains conflicts')

        first=host.open('randomNoteCommand',prefs=prefer)
        assert first['root']['type']=='Detail' and first['root']['props']['markdown']
        first=host.act('randomNoteCommand',action(first,'Copy File Path'))
        first_path=Path(host.effects[-1][1]['text']).resolve(strict=True)
        second=host.act('randomNoteCommand',action(first,'Another Note'))
        second=host.act('randomNoteCommand',action(second,'Copy File Path'))
        second_path=Path(host.effects[-1][1]['text']).resolve(strict=True)
        assert second_path!=first_path, (first_path,second_path)
        checks.append('randomNoteCommand opens actual note preview and rerolls without repeating the prior file')

        media=host.open('searchMedia',prefs={**prefer,'imageSize':'small'},args={'typeArgument':'png'})
        assert media['root']['type']=='Grid' and media['root']['props']['columns']==8
        assert [r['title'] for r in rows(media)]==['Picture.png']
        media=host.act('searchMedia',media['root']['props']['searchBarAccessory']['props']['actionId'],formValues={'value':'pdf'})
        assert [r['title'] for r in rows(media)]==['Paper.pdf']
        host.act('searchMedia',action(media,'Show in Explorer'))
        assert host.effects[-1][0]=='host.shell.reveal' and host.effects[-1][1]['path'].endswith('Paper.pdf')
        checks.append('searchMedia type filtering, grid density and platform-specific reveal')

        host.open('appendTaskCommand',prefs={**prefer,'notePath':'Tasks.md','heading':'Todo','noteTag':'#fixture','creationDate':True},args={'text':'Task body','dueDate':'2027-04-09'})
        task=(vault/'Tasks.md').read_text(encoding='utf-8')
        assert f'- [ ] #fixture Task body \U0001f4c5 2027-04-09 \u2795 {date.today()}' in task and task.endswith('# After\nintact\n')
        missing_task=vault/'Missing Tasks.md'
        blocked=host.open('appendTaskCommand','task-no-heading',
            {**prefer,'notePath':missing_task.name,'heading':'Todo'},args={'text':'Retained task draft'})
        assert field(blocked,'text')['defaultValue']=='Retained task draft'
        assert not missing_task.exists()
        checks.append('appendTaskCommand adds formatted task, due date and creation date under the requested heading')

        cap=host.open('quickCaptureCommand',prefs={**prefer,'todoNotePath':'Tasks.md','shoppingNotePath':'Shopping.md','dailyNotePath':'../outside.md'})
        cap=host.act('quickCaptureCommand',action(cap,'Save'),formValues={'type':'daily','text':'Must stay a draft'})
        assert field(cap,'text')['defaultValue']=='Must stay a draft'
        assert not vault.parent.joinpath('outside.md').exists()
        host.act('quickCaptureCommand',action(cap,'Save'),formValues={'type':'todo','text':'One item\ncontinued'})
        assert (vault/'Tasks.md').read_text(encoding='utf-8').endswith('- [ ] One item\n  continued')
        original_shopping=(vault/'Shopping.md').read_bytes()
        cap=host.open('quickCaptureCommand','capture-no-extension',{**prefer,'shoppingNotePath':'Shopping'})
        cap=host.act('capture-no-extension',action(cap,'Save'),formValues={'type':'shopping','text':'Retained capture'})
        assert field(cap,'text')['defaultValue']=='Retained capture'
        assert (vault/'Shopping.md').read_bytes()==original_shopping and not (vault/'Shopping').exists()
        checks.append('quickCaptureCommand preserves path-failure drafts and appends a multiline item only to an existing target')

        custom=host.open('runActionCommand',prefs=prefer)
        form=host.act('runActionCommand',action(custom,'Create New Action'))
        vault_key=field(form,'vault')['defaultValue']
        custom=host.act('runActionCommand',action(form,'Save Action'),formValues={'title':'Meeting','vault':vault_key,'path':'Logs/{content}','template':'Logged {content}','type':'capture','mode':'append','silent':True})
        form=host.act('runActionCommand',action(custom,'Run Action',row='Meeting'))
        host.act('runActionCommand',action(form,'Run Action'),formValues={'content':'Notes from standup'})
        assert (vault/'Logs/Notes_from_standup.md').read_text()=='Logged Notes from standup'
        custom=host.open('runActionCommand','custom-reopen',prefer)
        custom=host.act('custom-reopen',action(custom,'Export Actions'))
        assert json.loads(host.effects[-1][1]['text'])[0]['vaultPath']==vault_key
        host.clipboard=json.dumps([dict(id='imported-fixture',title='Imported',description='',vaultName='Notes',path='Shopping.md',template='{content}',type='capture',mode='prepend',heading='',silent=True)])
        custom=host.act('custom-reopen',action(custom,'Import Actions'))
        assert any(row['title']=='Imported' for row in rows(custom))
        custom=host.act('custom-reopen',action(custom,'Delete Action',row='Imported'))
        assert not any(row['title']=='Imported' for row in rows(custom))

        form=host.act('custom-reopen',action(custom,'Create New Action'))
        custom=host.act('custom-reopen',action(form,'Save Action'),formValues={'title':'Missing action heading',
            'vault':vault_key,'path':'Logs/Missing Heading','type':'capture','mode':'append','heading':'Todo','silent':True})
        form=host.act('custom-reopen',action(custom,'Run Action',row='Missing action heading'))
        blocked=host.act('custom-reopen',action(form,'Run Action'),formValues={'content':'Retained custom draft'})
        assert field(blocked,'content')['defaultValue']=='Retained custom draft'
        assert not (vault/'Logs/Missing Heading.md').exists()
        checks.append('runActionCommand creates, persists, runs, exports, imports and confirms deletion of saved actions')

        before=host.open('searchNoteCommand','trash',prefer,args={'searchArgument':'Trash Me'})
        host.confirm=False
        updated=host.act('trash',action(before,'Delete Note',row='Trash Me'))
        assert (vault/'Trash Me.md').exists()
        host.confirm=True
        host.act('trash',action(updated,'Delete Note',row='Trash Me'))
        assert not (vault/'Trash Me.md').exists()
        assert any(item.read_text()=='Owned trash fixture\n' for item in (vault/'.trash').iterdir())
        outside=root/'Outside.md'; put(outside,'Do not modify hard link target\n')
        os.link(outside,vault/'Outside Alias.md')
        alias=host.open('searchNoteCommand','hardlink',prefer,args={'searchArgument':'Outside Alias'})
        form=host.act('hardlink',action(alias,'Append to Note'))
        fail=host.act('hardlink',action(form,'Save'),formValues={'content':'Preserved hardlink draft'})
        assert field(fail,'content')['defaultValue']=='Preserved hardlink draft'
        assert outside.read_text()=='Do not modify hard link target\n'
        assert (duplicate/'Mirror/Alpha.md').read_text()=='Never change duplicate vault\n'
        checks.append('destructive delete requires confirmation, trash remains in vault and linked targets refuse writes')

        enabled=host.open('obsidianMenuBar',prefs={**prefer,'appendTemplate':'do not retain me'})
        assert enabled['mode']=='no-view' and host.leases['obsidian-vaults']=={'vaultPath':str(vault),'configFileName':'.fixture-settings'}
        def tray_id(title):
            return next(item['id'] for item in nodes({'children':host.tray['items']}) if item.get('title')==title)
        refresh=tray_id('Refresh Obsidian Menu')
        host.rpc('background.event',dict(resourceId='obsidian-vaults',actionId=refresh))
        assert 'Stale' in host.rpc('background.event',dict(resourceId='obsidian-vaults',actionId=refresh),error=True)
        host.rpc('background.event',dict(resourceId='obsidian-vaults',actionId=tray_id('Hide Obsidian Menu')))
        assert not host.leases and host.tray is None
    with session(binary,root,'obsidian-fixture-macos') as host:
        host.rpc('background.activate',dict(key='obsidian-vaults',data=prefer))
        assert host.tray and host.leases
        mac=host.open('searchNoteCommand',prefs=prefer,args={'searchArgument':'Alpha'})
        copy=action(mac,'Copy Note Content',path='Projects/')
        assert next(x['shortcut']['modifiers'] for x in mac['actions'] if x['id']==copy)==['cmd']
        assert action(mac,'Show in Finder',path='Projects/')
    checks.append('obsidianMenuBar durable registration, restored menu, stale revisions and removal, macOS URI/shortcuts')


def production_suite(archive,root,checks):
    package=root/'package'; package.mkdir()
    expected='bin/tezbar-obsidian'+('.exe' if sys.platform=='win32' else '')
    with zipfile.ZipFile(archive) as z:
        manifest=json.loads(z.read('extension.json'))
        assert set(z.namelist()) == set(manifest['files'])|{'extension.json'}
        for name,digest in manifest['files'].items():
            assert hashlib.sha256(z.read(name)).hexdigest()==digest
        binary=package/Path(expected).name
        put(binary,z.read(expected))
        binary.chmod(0o755)
    info=json.loads(subprocess.check_output([str(binary),'--protocol-info'],timeout=12))
    assert info['fixtureSupport'] is False and info['commands']==[x['name'] for x in MANIFEST['commands']]
    refused=subprocess.run([str(binary),'--fixtures',str(root)],capture_output=True,timeout=12)
    assert refused.returncode!=0 and not refused.stdout
    settings={'vaultPath':str(root/'missing-explicit-vault')}
    platform='aarch64-apple-darwin' if sys.platform=='darwin' else 'x86_64-pc-windows-msvc'
    with session(binary,root,platform,package,use_fixture=False) as host:
        for command in info['commands']:
            view=host.open(command,sid='stock-'+command,prefs=settings)
            assert view['mode'] in ['view','no-view']
        assert all(method in ['host.background.retain','host.tray.set'] for method,_ in host.effects),host.effects
    assert not (root/'missing-explicit-vault').exists()
    checks.append('Extracted production process opens all commands with an explicitly missing owned vault, no profile discovery or desktop effects, and refuses fixtures')


def main():
    p=argparse.ArgumentParser()
    source=p.add_mutually_exclusive_group(required=True)
    source.add_argument('--binary',type=Path)
    source.add_argument('--archive',type=Path)
    p.add_argument('--report',type=Path,required=True)
    options=p.parse_args()
    checks=[]
    with tempfile.TemporaryDirectory(prefix='tezbar-obsidian-owned-') as temp:
        root=Path(temp).resolve()
        if options.binary:
            fixture_suite(options.binary.resolve(),root,checks)
        else:
            production_suite(options.archive.resolve(),root,checks)
    evidence=dict(os=sys.platform,passed=len(checks),checks=checks,limitation='Owned synthetic vaults and recorded host effects. No real Obsidian or desktop acceptance.')
    result=json.dumps(evidence,indent=2)+'\n'
    options.report.parent.mkdir(parents=True,exist_ok=True)
    options.report.write_bytes(result.encode())
    print(result,end='')


if __name__=='__main__':
    main()
