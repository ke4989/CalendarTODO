use super::*;
use crate::adapters::icloud_caldav::{self, ICloudBinding};

pub(crate) struct ICloudDiscovery {
    pub binding: ICloudBinding,
    pub calendars: Vec<DiscoveredCalendarCollection>,
}

fn discovery_read(
    target: Url,
    depth: DavDepth,
    body: &[u8],
    executor: &dyn DavExecutor,
    authentication: &DavAuthentication,
) -> Result<crate::adapters::caldav_transport::DavResponse, ProviderReadError> {
    let policy =
        DavTransportPolicy::icloud_discovery(target.clone()).map_err(map_execution_error)?;
    let request = DavRequest::new(
        DavMethod::Propfind,
        target,
        body.to_vec(),
        depth,
        REQUEST_TIMEOUT_MS,
    )
    .map_err(map_execution_error)?;
    let response = policy
        .execute(executor, request, authentication)
        .map_err(map_execution_error)?;
    require_read_status(response.status())?;
    Ok(response)
}

pub(crate) fn discover_icloud(
    entry: &str,
    authentication: DavAuthentication,
    executor: Arc<dyn DavExecutor>,
) -> Result<ICloudDiscovery, ProviderReadError> {
    let bootstrap = icloud_caldav::parse(entry).map_err(map_execution_error)?;
    if !matches!(
        bootstrap.host_str(),
        Some("caldav.icloud.com" | "caldav.icloud.com.cn")
    ) || bootstrap.path() != "/"
    {
        return Err(ProviderReadError::UnsafeData);
    }
    let mut response = discovery_read(
        bootstrap.clone(),
        DavDepth::Zero,
        DISCOVERY_BODY,
        executor.as_ref(),
        &authentication,
    )?;
    let mut identity = parse_discovery_relaxed(response.body())?;
    let mut principal = icloud_caldav::resolve(response.final_url(), &identity.principal_href)
        .map_err(map_execution_error)?;
    if identity.calendar_home_href.is_empty() {
        response = discovery_read(
            principal,
            DavDepth::Zero,
            DISCOVERY_BODY,
            executor.as_ref(),
            &authentication,
        )?;
        principal = response.final_url().clone();
        identity.calendar_home_href = parse_discovery_document(response.body())?.calendar_home_href;
    }
    let home = icloud_caldav::resolve(response.final_url(), &identity.calendar_home_href)
        .map_err(map_execution_error)?;
    let response = discovery_read(
        home,
        DavDepth::One,
        CALENDARS_BODY,
        executor.as_ref(),
        &authentication,
    )?;
    let binding = ICloudBinding {
        bootstrap_url: bootstrap.to_string(),
        principal_url: principal.to_string(),
        home_url: response.final_url().to_string(),
        generation: 0,
    };
    binding.validate().map_err(map_execution_error)?;
    let calendars = parse_calendars(response.body())?
        .into_iter()
        .map(|calendar| {
            let url = binding
                .collection(&calendar.href)
                .map_err(map_execution_error)?;
            Ok(DiscoveredCalendarCollection {
                href: url.to_string(),
                display_name: calendar.display_name,
                server_color: calendar.color,
                privileges: calendar.privileges,
            })
        })
        .collect::<Result<Vec<_>, ProviderReadError>>()?;
    Ok(ICloudDiscovery { binding, calendars })
}

impl ExecutingCalDavAdapter {
    pub(crate) fn with_calendar_scope(mut self, scope: CalendarScopeReader) -> Self {
        self.calendar_scope = Some(scope);
        self
    }

    pub(crate) fn with_icloud_binding(
        mut self,
        binding: ICloudBinding,
    ) -> Result<Self, ProviderReadError> {
        self.entry_url = binding.validate().map_err(map_execution_error)?;
        self.policy = DavTransportPolicy::icloud_bound(
            &binding,
            self.context.calendar_ids_by_href.keys().cloned(),
        )
        .map_err(map_execution_error)?;
        self.icloud_binding = Some(binding);
        Ok(self)
    }

    /// The committed account home supplies metadata; each saved collection is
    /// independently enumerated. No entry/global shard is reconstructed.
    pub(super) fn collect_icloud_full(&self) -> Result<AccountInventoryStage, ProviderReadError> {
        let context = self.scoped_context()?;
        let binding = self.icloud_binding.as_ref().expect("bound source");
        let response = self.execute(
            DavMethod::Propfind,
            self.entry_url.clone(),
            CALENDARS_BODY.to_vec(),
            DavDepth::One,
        )?;
        require_read_status(response.status())?;
        let mut found = BTreeMap::new();
        for calendar in parse_calendars(response.body())? {
            let url = binding
                .collection(&calendar.href)
                .map_err(map_execution_error)?;
            if found.insert(url.to_string(), calendar).is_some() {
                return Err(ProviderReadError::UnsafeData);
            }
        }
        let mut calendars = Vec::new();
        for (route, calendar_id) in &context.calendar_ids_by_href {
            let collection = binding.collection(route).map_err(map_execution_error)?;
            let metadata = found
                .get(collection.as_str())
                .ok_or(ProviderReadError::Incomplete)?;
            let inventory = self.execute(
                DavMethod::Propfind,
                collection.clone(),
                INVENTORY_BODY.to_vec(),
                DavDepth::One,
            )?;
            require_read_status(inventory.status())?;
            let token = parse_optional_sync_token(inventory.body(), collection.as_str())?;
            let mut resources = Vec::new();
            for item in parse_inventory(inventory.body(), collection.as_str())? {
                let target =
                    icloud_caldav::resolve(&collection, &item.href).map_err(map_execution_error)?;
                if !resource_belongs_to_collection(collection.as_str(), &item.href) {
                    return Err(ProviderReadError::UnsafeData);
                }
                let fetched = self.execute(DavMethod::Get, target, Vec::new(), DavDepth::Zero)?;
                require_resource_status(fetched.status())?;
                if !fetched.content_type().is_some_and(|v| {
                    v.split(';')
                        .next()
                        .is_some_and(|v| v.trim().eq_ignore_ascii_case("text/calendar"))
                }) {
                    return Err(ProviderReadError::UnsafeData);
                }
                let raw = fetched.body().to_vec();
                let uid = extract_single_uid(&raw)?;
                resources.push(StagedResource {
                    href: item.href,
                    etag: staged_revision(fetched.etag(), &item.etag)?,
                    content_type: fetched.content_type().unwrap_or_default().to_owned(),
                    uid: uid.clone(),
                    parse_status: mutation_parse_status(&raw, &uid),
                    raw_ics: raw,
                });
            }
            calendars.push(CalendarInventoryStage {
                calendar_id: calendar_id.clone(),
                collection_url: route.clone(),
                display_name: metadata.display_name.clone(),
                server_color: metadata.color.clone(),
                privileges: metadata.privileges.join(","),
                sync_token: token,
                resources,
            });
        }
        Ok(AccountInventoryStage {
            account_id: context.account_id,
            provider_kind: context.provider_kind,
            user_label: context.user_label,
            credential_ref: context.credential_ref,
            region_profile: context.region_profile,
            complete: true,
            authoritative: true,
            observed_at_ms: self.observed_at_ms,
            calendars,
        })
    }
}
