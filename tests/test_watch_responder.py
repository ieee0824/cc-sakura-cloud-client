import importlib.util,json,pathlib,subprocess,tempfile,types,unittest
from unittest.mock import patch
spec=importlib.util.spec_from_file_location('responder',pathlib.Path(__file__).resolve().parents[1]/'skills/cc-sakura-watch/scripts/respond.py')
responder=importlib.util.module_from_spec(spec);spec.loader.exec_module(responder)

class ResponderTests(unittest.TestCase):
    def test_explanation_posts_once_without_forwarding_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            profile=pathlib.Path(directory)/'profile.toml'
            profile.write_text('project_url="https://example.org/owner/room"\n[auth]\nmember_id="me"\nprivate_key_file="private-marker"\n')
            args=types.SimpleNamespace(profile=str(profile),client='/fake/cc-sakura',codex='/fake/codex',model=None,timeout=120)
            event={'v':1,'room':'owner/room','self_member_id':'me','reason':'mention','message':{'id':123,'sender':'human','is_agent':False,'payload':{'message':'Explain the preceding post'},'auth':{'scheme':'session-v1','server_authenticated':True,'receipt':'private-marker'},'session':'private-marker'},'context':[{'id':122,'payload':{'message':'bounded evidence'},'profile':'private-marker'}]}
            posts=[]
            def execute(command,**kwargs):
                self.assertIsInstance(command,list);self.assertNotIn('shell',kwargs)
                if '--output-last-message' in command:
                    self.assertNotIn('private-marker',kwargs['input'])
                    self.assertIn('bounded evidence',kwargs['input'])
                    self.assertIn('--ignore-user-config',command)
                    self.assertIn('features.shell_tool=false',command)
                    pathlib.Path(command[command.index('--output-last-message')+1]).write_text(json.dumps({'reply':'Explanation of #122'}))
                    return subprocess.CompletedProcess(command,0)
                posts.append(command)
                return subprocess.CompletedProcess(command,0,stdout='{"id":500}')
            with patch.object(responder.subprocess,'run',side_effect=execute):
                self.assertEqual(responder.run(args,event),{'replied':500,'reply_to':123})
            self.assertEqual(len(posts),1)
            self.assertEqual(json.loads(posts[0][-1]),{'message':'Explanation of #122','reply_to':123})
            self.assertEqual(posts[0][-2],'owner/room')
            event['room']='another/room'
            with patch.object(responder.subprocess,'run') as run:
                with self.assertRaises(ValueError):responder.run(args,event)
                run.assert_not_called()
            event['room']='owner/room';event['message']['is_agent']=True
            with self.assertRaises(ValueError):responder.run(args,event)

    def test_no_reply_and_failure_never_send(self):
        with tempfile.TemporaryDirectory() as directory:
            profile=pathlib.Path(directory)/'profile.toml';profile.write_text('project_url="https://example.org/owner/room"\n[auth]\nmember_id="me"\n')
            args=types.SimpleNamespace(profile=str(profile),client='/fake/cc-sakura',codex='/fake/codex',model=None,timeout=120)
            event={'v':1,'room':'owner/room','self_member_id':'me','message':{'id':1,'sender':'human','auth':{'scheme':'signed-v1','verified':True},'payload':{'message':'thanks'}}}
            def skip(command,**kwargs):
                self.assertIn('--output-last-message',command)
                pathlib.Path(command[command.index('--output-last-message')+1]).write_text('{"reply":null}')
                return subprocess.CompletedProcess(command,0)
            with patch.object(responder.subprocess,'run',side_effect=skip) as run:
                self.assertEqual(responder.run(args,event),{'skipped':True});self.assertEqual(run.call_count,1)
            with patch.object(responder.subprocess,'run',return_value=subprocess.CompletedProcess([],1)) as run:
                with self.assertRaises(RuntimeError):responder.run(args,event)
                self.assertEqual(run.call_count,1)

if __name__=='__main__':unittest.main()
