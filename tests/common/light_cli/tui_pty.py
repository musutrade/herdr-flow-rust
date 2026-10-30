"""Real PTY journeys, no services. Also runnable against a frozen release binary."""
import os,sys,pathlib,tempfile,subprocess,json,pty,termios,fcntl,select,time,signal,sqlite3,hashlib
BIN=str(pathlib.Path(sys.argv[1]).resolve())
ROOT=pathlib.Path(tempfile.mkdtemp(prefix='light-175-pty-'));DATA=ROOT/'data'
ENV={'HOME':str(ROOT/'home'),'PATH':str(ROOT/'bin'),'TERM':'xterm','LANG':'C.UTF-8'}
if 'LLVM_PROFILE_FILE' in os.environ:ENV['LLVM_PROFILE_FILE']=os.environ['LLVM_PROFILE_FILE']
for d in ['home','bin','project1','project2']: (ROOT/d).mkdir()
for tool in ['herdr','git','gh','curl','xdg-open','herdr-flow']:
 p=ROOT/'bin'/tool;p.write_text('#!/bin/sh\necho called >> "'+str(ROOT/'calls')+'"\nexit 99\n');p.chmod(0o700)
legacy=ROOT/'home/.herdr/flow';legacy.mkdir(parents=True);(legacy/'queue.json').write_text('sentinel')
BASE=[BIN,'light','--data-root',str(DATA)]
def cli(args,body=None):
 p=subprocess.run(BASE+args,input=None if body is None else json.dumps(body),text=True,capture_output=True,env=ENV,timeout=10)
 assert p.returncode==0,(args,p.stderr)
 return json.loads(p.stdout)['data']
def req(key,change,refs=[]):return {'request_id':key,'reporter':'pty-user','summary':'summary '+key,'refs':refs,'change':change}
def add(key,project,batch,title):return cli(['task','add','--input','-'],req(key,{'kind':'create_task','task':{'project_id':project,'title':title,'source_ref':None,'batch_ref':batch,'reported_state':'reported-open'}}))
cli(['init'])
for i in [1,2]:cli(['project','register','--root',str(ROOT/f'project{i}'),'--label',f'Project-{i}'])
add('task-one',1,'alpha','First task');add('task-two',1,'beta','Second task');add('task-three',2,'alpha','Third task')
# Data containing terminal escapes is never interpreted by the browser.
add('task-four',2,'beta','unsafe\x1b]52;c;evil\x07\u202e'+'x'*170)
refs=[{'target':'https://example.invalid/result','commit':'abc123','result':'old-failure','recorded_at':'2026-10-30','sha256':'a'*64,'temporary':False},{'target':str(legacy/'queue.json'),'temporary':True}]
cli(['task','report','--input','-'],req('report-one',{'kind':'report','task_id':1,'expected_revision':1,'reported_state':'reported-review'},refs))
session={'agent':'codex','kind':'id','source':'fixture','value':'recorded-session'}
for i,role in enumerate(['developer','auditor']):
 cli(['binding','attach','--input','-'],req('bind-'+role,{'kind':'attach_binding','task_id':1,'expected_revision':2+i,'binding':{'role':role,'herdr_location':{'service':'offline','workspace':'fixture','pane':'fixture:p1'},'session':session if i==0 else None,'cwd':str(ROOT/'project1')}}))
query=['task','show','--project','1','--task','1'];before=cli(query)
# Seed an existing cache by an explicitly invoked observer against a nonexistent socket.
# The browser itself never invokes it.
cli(['observe','--once','--project','1','--task','1','--binding','1','--service','offline','--socket',str(ROOT/'absent.sock')])
conn=sqlite3.connect(DATA/'observations.sqlite3');snapshot=json.loads(conn.execute('select snapshot from latest where binding_id=1').fetchone()[0]);snapshot.update(observed_state='working',reason='controlled cached state',observed_at_ms=1,stale_after_ms=1);conn.execute('update latest set snapshot=? where binding_id=1',(json.dumps(snapshot),));conn.commit();conn.close()
def digest():
 result={}
 for filename in ['records.sqlite3','observations.sqlite3']:
  c=sqlite3.connect('file:'+str(DATA/filename)+'?mode=ro',uri=True);result[filename]='\n'.join(c.iterdump());c.close()
 return result
initial=digest();TRANSCRIPTS=[]
class Terminal:
 def __init__(self,extra=[],trace=False,data=None):
  self.master,self.slave=pty.openpty();self.original=termios.tcgetattr(self.slave);self.buf=b'';self.all=b''
  argv=([BIN,'light','--data-root',str(data)] if data else BASE)+['tui','--limit','1']+extra
  if trace:argv=['/usr/bin/strace','-f','-qq','-yy','-s','4096','-e','trace=%file,%process,%network,getdents64','-o',str(ROOT/'tui.strace'),'--']+argv
  self.p=subprocess.Popen(argv,stdin=self.slave,stdout=self.slave,stderr=self.slave,env=ENV,cwd=ROOT/'project1',start_new_session=True,preexec_fn=lambda:fcntl.ioctl(0,termios.TIOCSCTTY,0))
 def screen(self):
  deadline=time.monotonic()+12
  while b'light> ' not in self.buf:
   assert time.monotonic()<deadline,('no prompt',self.all[-5000:])
   if select.select([self.master],[],[],.1)[0]:
    data=os.read(self.master,65536);assert data;self.buf+=data;self.all+=data
   assert self.p.poll() is None or b'light> ' in self.buf,self.all
  result,self.buf=self.buf.split(b'light> ',1)
  assert b'\x1b' not in result,('terminal control injection',result)
  return result.decode('utf-8')
 def cmd(self,text):os.write(self.master,text.encode()+b'\n');return self.screen()
 def close(self,mode='quit'):
  if mode=='quit':os.write(self.master,b'quit\n')
  elif mode=='eof':os.write(self.master,b'\x04')
  elif mode=='ctrl-c':os.write(self.master,b'\x03')
  else:os.kill(self.p.pid,signal.SIGTERM)
  code=self.p.wait(timeout=10)
  assert termios.tcgetattr(self.slave)==self.original,mode
  assert code==(0 if mode in ['quit','eof'] else -signal.SIGINT if mode=='ctrl-c' else -signal.SIGTERM),(mode,code)
  os.close(self.master);os.close(self.slave);TRANSCRIPTS.append(self.all.decode('utf-8'))
try:
 t=Terminal(trace=True);assert 'First task' in t.screen()
 assert 'Second task' in t.cmd('next');assert 'First task' in t.cmd('prev')
 assert 'no previous page' in t.cmd('prev');assert 'not on the displayed page' in t.cmd('open 99')
 assert 'open a task first' in t.cmd('bindings');assert 'unknown/incomplete' in t.cmd('nonsense')
 assert 'expected a positive ID' in t.cmd('project -1');assert 'unexpected argument' in t.cmd('quit extra')
 assert 'at most 256' in t.cmd('batch '+'x'*257)
 assert 'Project-1' in t.cmd('projects');assert 'Project-2' in t.cmd('next')
 assert 'Third task' in t.cmd('open 2');assert 'unsafe' in t.cmd('next')
 assert 'No records' in t.cmd('next');assert 'no next page' in t.cmd('next')
 assert 'unsafe' in t.cmd('prev')
 assert 'First task' in t.cmd('project all');assert 'Second task' in t.cmd('batch beta')
 assert 'unsafe' in t.cmd('next');assert 'No records' in t.cmd('batch missing')
 assert 'First task' in t.cmd('batch -');assert 'First task' in t.cmd('project 1')
 out=t.cmd('open 1');assert 'Agent REPORT (not observation)' in out and 'reported-review' in out and 'summary bind-auditor' in out
 assert str(before['task']['revision']) in out
 assert 'create_task' in out;assert 'report' in t.cmd('next')
 event_id=before['events']['items'][1]['id'];out=t.cmd('open '+str(event_id))
 for ref in refs:
  assert ref['target'] in out
 assert 'abc123' in out and 'old-failure' in out and '"temporary": true' in out
 assert 'no next page' in t.cmd('next')
 assert 'developer' in t.cmd('bindings');out=t.cmd('open 1')
 assert 'recorded-session' in out and '"recorded_state": "working"' in out
 assert '"observed_state": "unknown"' in out and '"fresh": false' in out and 'not proof of current state' in out
 assert 'developer' in t.cmd('bindings');assert 'auditor' in t.cmd('next');out=t.cmd('open 2')
 assert '"session": null' in out and 'no_cached_observation' in out
 assert 'create_task' in t.cmd('events');assert 'References are text only' in t.cmd('help')
 assert 'First task' in t.cmd('tasks');t.close()
 # Filtered launch, refresh after external append, EOF and signals retain terminal state.
 for mode in ['eof','ctrl-c','sigterm']:
  t=Terminal(['--project','2','--batch','alpha']);assert 'Third task' in t.screen();t.close(mode)
 t=Terminal();t.screen();os.rename(DATA/'records.sqlite3',DATA/'held.sqlite3')
 assert 'Data error:' in t.cmd('refresh');assert not (DATA/'records.sqlite3').exists()
 os.rename(DATA/'held.sqlite3',DATA/'records.sqlite3');assert 'First task' in t.cmd('r');t.close()
 t=Terminal(data=ROOT/'missing');assert 'Data error:' in t.screen();t.close();assert not (ROOT/'missing').exists()
 # Unavailable cache is an explicit unknown/error, not a browser initialization.
 os.rename(DATA/'observations.sqlite3',DATA/'held-cache.sqlite3')
 t=Terminal();t.screen();t.cmd('open 1');t.cmd('bindings');out=t.cmd('open 1');assert 'cache_error' in out;t.close()
 assert not (DATA/'observations.sqlite3').exists();os.rename(DATA/'held-cache.sqlite3',DATA/'observations.sqlite3')
 assert cli(query)==before;assert digest()==initial;assert not (ROOT/'calls').exists()
 trace=(ROOT/'tui.strace').read_text()
 for forbidden in ['clone(','clone3(','fork(','vfork(','connect(','socket(','getdents64(',str(legacy/'queue.json')]:assert forbidden not in trace,forbidden
 assert trace.count('execve(')==1
 assert (legacy/'queue.json').read_text()=='sentinel'
 piped=subprocess.run(BASE+['tui'],input='',capture_output=True,text=True,env=ENV);assert piped.returncode==2 and 'requires terminal' in piped.stderr
 result={'status':'passed','binary_sha256':hashlib.sha256(pathlib.Path(BIN).read_bytes()).hexdigest(),'terminal_modes_restored':['quit','EOF','Ctrl-C','SIGTERM'],'logical_databases_unchanged':True,'references_not_opened':True,'trace_no_process_network_scan':True,'transcripts':TRANSCRIPTS}
 if len(sys.argv)>2:pathlib.Path(sys.argv[2]).write_text(json.dumps(result,indent=2)+'\n')
 print(json.dumps({k:v for k,v in result.items() if k!='transcripts'}))
finally:
 for p in [locals().get('t')]:
  if p and p.p.poll() is None:p.p.kill();p.p.wait()
 import shutil;shutil.rmtree(ROOT)
