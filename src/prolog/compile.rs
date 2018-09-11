use prolog_parser::ast::*;

use prolog::instructions::*;
use prolog::debray_allocator::*;
use prolog::codegen::*;
use prolog::machine::*;
use prolog::toplevel::*;

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
use std::mem;

#[allow(dead_code)]
fn print_code(code: &Code) {
    for clause in code {
        match clause {
            &Line::Arithmetic(ref arith) =>
                println!("{}", arith),
            &Line::Fact(ref fact) =>
                for fact_instr in fact {
                    println!("{}", fact_instr);
                },
            &Line::Cut(ref cut) =>
                println!("{}", cut),
            &Line::Choice(ref choice) =>
                println!("{}", choice),
            &Line::Control(ref control) =>
                println!("{}", control),
            &Line::IndexedChoice(ref choice) =>
                println!("{}", choice),
            &Line::Indexing(ref indexing) =>
                println!("{}", indexing),
            &Line::Query(ref query) =>
                for query_instr in query {
                    println!("{}", query_instr);
                }
        }
    }
}

pub fn parse_code(wam: &mut Machine, buffer: &str) -> Result<TopLevelPacket, ParserError>
{
    let atom_tbl = wam.atom_tbl();
    let flags = wam.machine_flags();

    let indices = machine_code_indices!(&mut wam.code_dir, &mut wam.op_dir, &mut HashMap::new());

    let mut worker = TopLevelWorker::new(buffer.as_bytes(), atom_tbl, flags, indices);
    worker.parse_code()
}

pub fn compile_term(wam: &mut Machine, term: Term) -> Result<TopLevelPacket, ParserError> {
    let indices = machine_code_indices!(&mut wam.code_dir, &mut wam.op_dir, &mut HashMap::new());
    parse_term(term, indices)
}

// throw errors if declaration or query found.
fn compile_relation(tl: &TopLevel, non_counted_bt: bool, flags: MachineFlags) -> Result<Code, ParserError>
{
    let mut cg = CodeGenerator::<DebrayAllocator>::new(non_counted_bt, flags);

    match tl {
        &TopLevel::Declaration(_) | &TopLevel::Query(_) =>
            Err(ParserError::ExpectedRel),
        &TopLevel::Predicate(ref clauses) =>
            cg.compile_predicate(&clauses.0),
        &TopLevel::Fact(ref fact) =>
            Ok(cg.compile_fact(fact)),
        &TopLevel::Rule(ref rule) =>
            cg.compile_rule(rule)
    }
}

// set first jmp_by_call or jmp_by_index instruction to code.len() -
// idx, where idx is the place it occurs. It only does this to the
// *first* uninitialized jmp index it encounters, then returns.
fn set_first_index(code: &mut Code)
{
    let code_len = code.len();

    for (idx, line) in code.iter_mut().enumerate() {
        match line {
            &mut Line::Control(ControlInstruction::JmpBy(_, ref mut offset, ..)) if *offset == 0 => {
                *offset = code_len - idx;
                break;
            },
            _ => {}
        };
    }
}

fn compile_appendix(code: &mut Code, queue: Vec<TopLevel>, non_counted_bt: bool, flags: MachineFlags)
                    -> Result<(), ParserError>
{
    for tl in queue.iter() {
        set_first_index(code);
        code.append(&mut compile_relation(tl, non_counted_bt, flags)?);
    }

    Ok(())
}

fn compile_query(terms: Vec<QueryTerm>, queue: Vec<TopLevel>, flags: MachineFlags)
                 -> Result<(Code, AllocVarDict), ParserError>
{
    // count backtracking inferences.
    let mut cg = CodeGenerator::<DebrayAllocator>::new(false, flags);
    let mut code = try!(cg.compile_query(&terms));

    compile_appendix(&mut code, queue, false, flags)?;

    Ok((code, cg.take_vars()))
}

fn compile_decl(wam: &mut Machine, tl: TopLevel, queue: Vec<TopLevel>) -> EvalSession
{
    match tl {
        TopLevel::Declaration(Declaration::Op(op_decl)) => {
            try_eval_session!(op_decl.submit(clause_name!("user"), &mut wam.op_dir));
            EvalSession::EntrySuccess
        },
        TopLevel::Declaration(Declaration::UseModule(name)) =>
            wam.use_module_in_toplevel(name),
        TopLevel::Declaration(Declaration::UseQualifiedModule(name, exports)) =>
            wam.use_qualified_module_in_toplevel(name, exports),
        TopLevel::Declaration(_) =>
            EvalSession::from(ParserError::InvalidModuleDecl),
        _ => {
            let name = try_eval_session!(if let Some(name) = tl.name() {
                match ClauseType::from(name.clone(), tl.arity(), None) {
                    ClauseType::Named(..) | ClauseType::Op(..) =>
                        Ok(name),
                    _ => {
                        let err_str = format!("{}/{}", name.as_str(), tl.arity());
                        Err(SessionError::ImpermissibleEntry(err_str))
                    }
                }
            } else {
                Err(SessionError::NamelessEntry)
            });

            let mut code = try_eval_session!(compile_relation(&tl, false, wam.machine_flags()));
            try_eval_session!(compile_appendix(&mut code, queue, false, wam.machine_flags()));

            if !code.is_empty() {
                wam.add_user_code(name, tl.arity(), code)
            } else {
                EvalSession::from(SessionError::ImpermissibleEntry(String::from("no code generated.")))
            }
        }
    }
}

pub fn compile_packet(wam: &mut Machine, tl: TopLevelPacket) -> EvalSession
{
    match tl {
        TopLevelPacket::Query(terms, queue) =>
            match compile_query(terms, queue, wam.machine_flags()) {
                Ok((mut code, vars)) => wam.submit_query(code, vars),
                Err(e) => EvalSession::from(e)
            },
        TopLevelPacket::Decl(tl, queue) =>
            compile_decl(wam, tl, queue)
    }
}

pub struct ListingCompiler<'a> {
    wam: &'a mut Machine,
    non_counted_bt_preds: HashSet<PredicateKey>,
    module: Option<Module>
}

impl<'a> ListingCompiler<'a> {
    pub fn new(wam: &'a mut Machine) -> Self {
        ListingCompiler { wam,
                          module: None,
                          non_counted_bt_preds: HashSet::new() }
    }

    fn get_module_name(&self) -> ClauseName {
        self.module.as_ref()
            .map(|module| module.module_decl.name.clone())
            .unwrap_or(ClauseName::BuiltIn("user"))
    }

    fn generate_code(&mut self, decls: Vec<(Predicate, VecDeque<TopLevel>)>, code_dir: &mut CodeDir)
                     -> Result<Code, SessionError>
    {
        let mut code = vec![];

        for (decl, queue) in decls {
            let (name, arity) = decl.0.first().and_then(|cl| {
                let arity = cl.arity();
                cl.name().map(|name| (name, arity))
            }).ok_or(SessionError::NamelessEntry)?;

            let non_counted_bt = self.non_counted_bt_preds.contains(&(name.clone(), arity));

            let p = code.len() + self.wam.code_size();
            let mut decl_code = compile_relation(&TopLevel::Predicate(decl), non_counted_bt,
                                                 self.wam.machine_flags())?;

            compile_appendix(&mut decl_code, Vec::from(queue), non_counted_bt,
                             self.wam.machine_flags())?;

            let idx = code_dir.entry((name, arity)).or_insert(CodeIndex::default());
            set_code_index!(idx, IndexPtr::Index(p), self.get_module_name());

            code.extend(decl_code.into_iter());
        }

        Ok(code)
    }

    fn add_code(self, code: Code, indices: MachineCodeIndices) {
        let code_dir = mem::replace(indices.code_dir, HashMap::new());
        let op_dir   = mem::replace(indices.op_dir, HashMap::new());

        if let Some(mut module) = self.module {
            module.code_dir.extend(as_module_code_dir(code_dir));
            module.op_dir.extend(op_dir.into_iter());

            self.wam.add_module(module, code);
        } else {
            self.wam.add_batched_code(code, code_dir);
            self.wam.add_batched_ops(op_dir);
        }
    }

    fn add_non_counted_bt_flag(&mut self, name: ClauseName, arity: usize) {
        self.non_counted_bt_preds.insert((name, arity));
    }

    fn process_decl(&mut self, decl: Declaration, indices: &mut MachineCodeIndices)
                    -> Result<(), SessionError>
    {
        match decl {
            Declaration::NonCountedBacktracking(name, arity) =>
                Ok(self.add_non_counted_bt_flag(name, arity)),
            Declaration::Op(op_decl) =>
                op_decl.submit(self.get_module_name(), &mut indices.op_dir),
            Declaration::UseModule(name) =>
                if let Some(ref submodule) = self.wam.get_module(name.clone()) {
                    Ok(use_module(&mut self.module, submodule, indices))
                } else {
                    Err(SessionError::ModuleNotFound)
                },
            Declaration::UseQualifiedModule(name, exports) =>
                if let Some(ref submodule) = self.wam.get_module(name.clone()) {
                    Ok(use_qualified_module(&mut self.module, submodule, &exports, indices))
                } else {
                    Err(SessionError::ModuleNotFound)
                },
            Declaration::Module(module_decl) =>
                if self.module.is_none() {
                    // worker.source_mod = module_decl.name.clone();
                    self.module = Some(Module::new(module_decl));
                    Ok(())
                } else {
                    Err(SessionError::from(ParserError::InvalidModuleDecl))
                }
        }
    }
}

fn use_module(module: &mut Option<Module>, submodule: &Module, indices: &mut MachineCodeIndices)
{
    indices.use_module(submodule);

    if let &mut Some(ref mut module) = module {
        module.use_module(submodule);
    }
}

fn use_qualified_module(module: &mut Option<Module>, submodule: &Module, exports: &Vec<PredicateKey>,
                        indices: &mut MachineCodeIndices)
{
    indices.use_qualified_module(submodule, exports);

    if let &mut Some(ref mut module) = module {
        module.use_qualified_module(submodule, exports);
    }
}

pub
fn compile_listing<R: Read>(wam: &mut Machine, src: R, mut indices: MachineCodeIndices) -> EvalSession
{
    let mut worker = TopLevelBatchWorker::new(src, wam.atom_tbl(), wam.machine_flags());
    let mut compiler = ListingCompiler::new(wam);

    while let Some(decl) = try_eval_session!(worker.consume(&mut indices)) {
        try_eval_session!(compiler.process_decl(decl, &mut indices));
    }

    let code = try_eval_session!(compiler.generate_code(worker.results, &mut indices.code_dir));
    compiler.add_code(code, indices);

    EvalSession::EntrySuccess
}

pub fn compile_user_module<R: Read>(wam: &mut Machine, src: R) -> EvalSession {
    let mut indices = machine_code_indices!(&mut CodeDir::new(), &mut default_op_dir(),
                                            &mut HashMap::new());

    if let Some(ref builtins) = wam.modules.get(&clause_name!("builtins")) {
        indices.use_module(builtins);
    } else {
        return EvalSession::from(SessionError::ModuleNotFound);
    }

    compile_listing(wam, src, indices)
}
