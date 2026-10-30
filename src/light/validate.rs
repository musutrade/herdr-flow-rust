use super::*;

pub(super) fn text(value: &str, min: usize, max: usize) -> Result<()> {
    if value.len() < min || value.len() > max || value.contains('\0') {
        return Err(Error::Invalid(format!(
            "text length must be {min}..={max} bytes, without NUL"
        )));
    }
    Ok(())
}

fn optional(value: &Option<String>, max: usize) -> Result<()> {
    if let Some(s) = value {
        text(s, 0, max)?;
    }
    Ok(())
}

pub(super) fn page(p: Page) -> Result<()> {
    if p.after_id < 0 || !(1..=100).contains(&p.limit) {
        return Err(Error::Invalid("invalid page".into()));
    }
    Ok(())
}

pub(super) fn request(r: &Request) -> Result<String> {
    text(&r.request_id, 1, 256)?;
    text(&r.reporter, 1, 256)?;
    text(&r.summary, 0, 8192)?;
    references(&r.refs)?;
    change(&r.change)?;
    let json = serde_json::to_string(r)?;
    text(&json, 1, 65536)?;
    Ok(json)
}

fn change(c: &Change) -> Result<()> {
    match c {
        Change::CreateTask { task } => {
            text(&task.title, 1, 512)?;
            optional(&task.source_ref, 4096)?;
            optional(&task.batch_ref, 256)?;
            text(&task.reported_state, 1, 128)
        }
        Change::Report { reported_state, .. } => text(reported_state, 1, 128),
        Change::AttachBinding { binding, .. } | Change::UpdateBinding { binding, .. } => {
            validate_binding(binding)
        }
        Change::SetResponsible { .. } => Ok(()),
    }
}

fn validate_binding(b: &BindingInput) -> Result<()> {
    text(&b.role, 1, 128)?;
    text(&b.herdr_location.service, 1, 256)?;
    text(&b.herdr_location.workspace, 1, 256)?;
    text(&b.herdr_location.pane, 1, 256)?;
    text(&b.cwd, 1, 4096)?;
    if !std::path::Path::new(&b.cwd).is_absolute() {
        return Err(Error::Invalid("binding cwd must be absolute".into()));
    }
    if let Some(s) = &b.session {
        for field in [&s.agent, &s.kind, &s.source, &s.value] {
            text(field, 1, 1024)?;
        }
    }
    Ok(())
}

fn references(refs: &[Reference]) -> Result<()> {
    if refs.len() > 32 {
        return Err(Error::Invalid("at most 32 refs".into()));
    }
    for r in refs {
        text(&r.target, 1, 4096)?;
        if !valid_target(&r.target) {
            return Err(Error::Invalid(
                "ref must be absolute path or http(s) URL".into(),
            ));
        }
        optional(&r.commit, 128)?;
        optional(&r.result, 256)?;
        optional(&r.recorded_at, 128)?;
        optional(&r.sha256, 64)?;
    }
    text(&serde_json::to_string(refs)?, 0, 32768)
}

fn valid_target(s: &str) -> bool {
    std::path::Path::new(s).is_absolute() || s.starts_with("https://") || s.starts_with("http://")
}
